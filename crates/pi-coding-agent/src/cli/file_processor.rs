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
//! Images go through [`crate::images::process_image`] with
//! `autoResizeImages: false` (the print path's setting) and the default
//! [`crate::images::default_transformer`]: supported formats pass through
//! byte-for-byte, while bmp is decoded and re-encoded as PNG so it attaches
//! instead of taking upstream's could-not-convert text. Detection is shared
//! with the read tool via [`crate::images::detect_supported_image_format`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pi_ai::types::ImageContent;

use crate::images::{
    ImageProcessResult, ImageSettings, ResizeOptions, default_transformer,
    detect_supported_image_format, to_image_content,
};
use crate::tools::path_utils::resolve_read_path;

/// `processFileArguments` (`file-processor.ts:25-88`): text blocks and image
/// attachments, in argument order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ImageContent>,
}

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
        if let Some(format) = detect_supported_image_format(&bytes) {
            // The print path passes `autoResizeImages: false` (`main.ts:270`),
            // so supported formats are attached with their raw bytes.
            let settings = ImageSettings {
                auto_resize: false,
                ..ImageSettings::default()
            };
            match crate::images::process_image(
                &bytes,
                format.mime_type(),
                &settings,
                &ResizeOptions::default(),
                default_transformer(),
            ) {
                ImageProcessResult::Processed {
                    data,
                    mime_type,
                    hints,
                } => {
                    processed.images.push(to_image_content(data, mime_type));
                    let hints = hints.join("\n");
                    let _ = writeln!(
                        processed.text,
                        "<file name=\"{}\">{hints}</file>",
                        absolute.display()
                    );
                }
                ImageProcessResult::Failed { message } => {
                    // BMP and other unconvertible formats take the same
                    // omission path as upstream's missing sharp backend.
                    let _ = writeln!(
                        processed.text,
                        "<file name=\"{}\">{message}</file>",
                        absolute.display()
                    );
                }
            }
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

#[cfg(test)]
mod tests {
    // `assert!(v.is_empty())` stays: the `assert_eq!(v, [])` alternative is
    // noisier for these fixture assertions.
    #![allow(clippy::assert_is_empty)]
    use super::*;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

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
    fn malformed_bmp_reports_the_upstream_omission_text() {
        let dir = tempdir();
        // Header-only BMP: the sniffer accepts it, but there is no pixel data,
        // so even the real backend cannot decode it (Photon-unavailable path).
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
            processed.text.contains(crate::images::UNCONVERTIBLE_IMAGE),
            "{processed:?}"
        );
    }

    #[test]
    fn gif_and_webp_args_attach_images_in_argument_order() {
        let dir = tempdir();
        // Header-valid synthetic files; with autoResizeImages: false the
        // images module attaches the bytes unchanged.
        let gif = b"GIF89a\x01\x00\x01\x00";
        let mut webp = b"RIFF\x16\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"VP8L\x0a\x00\x00\x00\x2f\x00\x00\x00\x00");
        let gif_path = write(&dir, "image.gif", gif);
        let webp_path = write(&dir, "image.webp", &webp);

        let processed = process_file_arguments(
            &["image.gif".to_string(), "image.webp".to_string()],
            dir.path(),
        )
        .expect("ok");

        assert_eq!(processed.images.len(), 2);
        assert_eq!(processed.images[0].mime_type, "image/gif");
        assert_eq!(processed.images[0].data, BASE64_STANDARD.encode(gif));
        assert_eq!(processed.images[1].mime_type, "image/webp");
        assert_eq!(processed.images[1].data, BASE64_STANDARD.encode(&webp));
        assert_eq!(
            processed.text,
            format!(
                "<file name=\"{}\"></file>\n<file name=\"{}\"></file>\n",
                gif_path.display(),
                webp_path.display()
            )
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

    #[test]
    fn valid_bmp_is_converted_to_a_png_attachment() {
        use image::codecs::bmp::BmpEncoder;
        use image::{ExtendedColorType, ImageEncoder, RgbImage};

        let dir = tempdir();
        let source = RgbImage::new(2, 2);
        let mut bmp = Vec::new();
        BmpEncoder::new(&mut bmp)
            .write_image(
                source.as_raw(),
                source.width(),
                source.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode synthetic bmp");
        let path = write(&dir, "image.bmp", &bmp);

        let processed = process_file_arguments(&["image.bmp".to_string()], dir.path()).expect("ok");
        assert_eq!(processed.images.len(), 1);
        assert_eq!(processed.images[0].mime_type, "image/png");
        let decoded = image::load_from_memory(
            &BASE64_STANDARD
                .decode(&processed.images[0].data)
                .expect("base64"),
        )
        .expect("decode converted png");
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(
            processed.text,
            format!(
                "<file name=\"{}\">[Image converted from image/bmp to image/png.]</file>\n",
                path.display()
            )
        );
    }
}

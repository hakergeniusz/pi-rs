//! Self-contained image attachment / input pipeline.
//!
//! Ground truth for the ported semantics:
//! - TS `packages/coding-agent/src/utils/image-process.ts:1-119`
//!   (`processImage`, MIME normalization, the two omission messages).
//! - TS `packages/coding-agent/src/core/tools/read.ts:27-38,95-107,275-294`
//!   (`ReadToolOutput`, `getNonVisionImageNote`, the image/note content blocks).
//! - TS `packages/coding-agent/src/utils/tool-result-images.ts:1-67`
//!   (`normalizeToolResultImages`).
//! - TS `packages/coding-agent/src/core/sdk.ts:296-331` (`blockImages`
//!   `convertToLlm` filter) and `core/settings-defaults.ts:35-44` (defaults).
//!
//! [`sniff`] detects supported formats from magic bytes and reads dimensions
//! from headers; [`resize`] ports the resize policy and [`transform`] supplies
//! the real decode/Lanczos3-resize/encode backend (the default for hosts).
//! Terminal drawing stays in pi-tui (`terminal_image.rs`, `components/image.rs`)
//! — see `WIRING.md` for host call sites and deliberate deviations.

pub mod resize;
pub mod sniff;
pub mod transform;

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use pi_agent::agent_message::AgentMessage;
use pi_ai::types::{ContentBlock, ImageContent, InputContent, TextContent};

use crate::config::settings::{
    DEFAULT_IMAGES_AUTO_RESIZE, DEFAULT_IMAGES_BLOCK_IMAGES, DEFAULT_TERMINAL_IMAGE_WIDTH_CELLS,
    DEFAULT_TERMINAL_SHOW_IMAGES, SettingsManager,
};

pub use resize::{
    DEFAULT_JPEG_QUALITY, DEFAULT_MAX_BYTES, DEFAULT_MAX_HEIGHT, DEFAULT_MAX_WIDTH,
    ImageTransformer, ResizeOptions, ResizedImage, UnavailableTransformer,
};
pub use sniff::{ImageFormat, detect_supported_image_format};
pub use transform::{ImageCrateTransformer, default_transformer};

/// `image-process.ts:82`: unsupported input could not be converted to PNG.
pub const UNCONVERTIBLE_IMAGE: &str =
    "[Image omitted: could not be converted to a supported inline image format.]";
/// `image-process.ts:91`: no encoded candidate fit under `maxBytes`.
pub const COULD_NOT_RESIZE: &str =
    "[Image omitted: could not be resized below the inline image size limit.]";
/// `read.ts:106`: the model has no image input.
pub const NON_VISION_NOTE: &str =
    "[Current model does not support images. The image will be omitted from this request.]";
/// `sdk.ts:312`: text replacing an image when `blockImages` is enabled.
pub const IMAGE_READING_DISABLED: &str = "Image reading is disabled.";

/// Snapshot of the four image settings (`settings-defaults.ts:35-44`,
/// `settings-manager.ts:1208-1236,1326-1350`). The getters already normalize
/// the values (width clamped to `>= 1`, booleans fall back on wrong types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSettings {
    /// `terminal.showImages` — whether the host draws images in terminal output.
    pub show_images: bool,
    /// `terminal.imageWidthCells` — display width in terminal cells, min 1.
    pub image_width_cells: i64,
    /// `images.autoResize` — resize images to inline provider limits.
    pub auto_resize: bool,
    /// `images.blockImages` — strip images before they reach the provider.
    pub block_images: bool,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            show_images: DEFAULT_TERMINAL_SHOW_IMAGES,
            image_width_cells: DEFAULT_TERMINAL_IMAGE_WIDTH_CELLS,
            auto_resize: DEFAULT_IMAGES_AUTO_RESIZE,
            block_images: DEFAULT_IMAGES_BLOCK_IMAGES,
        }
    }
}

impl ImageSettings {
    /// Build a snapshot from the live settings manager (the host wiring point).
    #[must_use]
    pub fn from_settings(manager: &SettingsManager) -> Self {
        Self {
            show_images: manager.get_show_images(),
            image_width_cells: manager.get_image_width_cells(),
            auto_resize: manager.get_image_auto_resize(),
            block_images: manager.get_block_images(),
        }
    }

    /// The settings selector's width clamp (`settings-manager.ts:1229-1236`).
    #[must_use]
    pub fn clamped_width_cells(width: i64) -> i64 {
        width.max(1)
    }
}

/// `baseMimeType` (`image-process.ts:29-31`, `clipboard-image.ts:26-28`).
#[must_use]
pub fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_lowercase()
}

/// `normalizeSupportedImageMimeType` (`image-process.ts:33-47`): only the
/// four formats the providers inline directly survive here; BMP and others
/// go through the PNG conversion seam.
#[must_use]
pub fn normalize_supported_image_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

/// `ProcessImageResult` (`image-process.ts:11-21`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageProcessResult {
    Processed {
        data: String,
        mime_type: String,
        hints: Vec<String>,
    },
    Failed {
        message: String,
    },
}

impl ImageProcessResult {
    /// Whether an image block can be built from this result.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Processed { .. })
    }
}

/// `NormalizedImage` (`image-process.ts:23-27`).
struct NormalizedImage {
    bytes: Vec<u8>,
    mime_type: String,
    converted_from: Option<String>,
}

/// `normalizeImage` (`image-process.ts:49-65`).
fn normalize_image(
    bytes: &[u8],
    mime_type: &str,
    transformer: &dyn ImageTransformer,
) -> Option<NormalizedImage> {
    if let Some(mime) = normalize_supported_image_mime_type(mime_type) {
        return Some(NormalizedImage {
            bytes: bytes.to_vec(),
            mime_type: mime.to_string(),
            converted_from: None,
        });
    }
    let png = transformer.convert_to_png(bytes)?;
    Some(NormalizedImage {
        bytes: png,
        mime_type: "image/png".to_string(),
        converted_from: Some(base_mime_type(mime_type)),
    })
}

/// `conversionHint` (`image-process.ts:67-70`).
fn conversion_hint(from: Option<&str>, to: &str) -> Option<String> {
    from.filter(|from| *from != to)
        .map(|from| format!("[Image converted from {from} to {to}.]"))
}

/// Port of `processImage` (`image-process.ts:72-119`). `resize_options` is the
/// model's profile (or the conservative defaults); `settings.auto_resize`
/// toggles the resize path exactly like upstream's `autoResizeImages`. Normal
/// callers pass [`default_transformer`] (the host's Photon replacement); pass
/// [`UnavailableTransformer`] only to exercise upstream's omission paths.
pub fn process_image(
    bytes: &[u8],
    mime_type: &str,
    settings: &ImageSettings,
    resize_options: &ResizeOptions,
    transformer: &dyn ImageTransformer,
) -> ImageProcessResult {
    let Some(normalized) = normalize_image(bytes, mime_type, transformer) else {
        return ImageProcessResult::Failed {
            message: UNCONVERTIBLE_IMAGE.to_string(),
        };
    };

    if !settings.auto_resize {
        let mut hints = Vec::new();
        if let Some(hint) =
            conversion_hint(normalized.converted_from.as_deref(), &normalized.mime_type)
        {
            hints.push(hint);
        }
        return ImageProcessResult::Processed {
            data: BASE64_STANDARD.encode(&normalized.bytes),
            mime_type: normalized.mime_type,
            hints,
        };
    }

    let Some(format) = sniff::detect_supported_image_format(&normalized.bytes) else {
        return ImageProcessResult::Failed {
            message: COULD_NOT_RESIZE.to_string(),
        };
    };
    let Some(dims) = sniff::image_dimensions(&normalized.bytes, format) else {
        return ImageProcessResult::Failed {
            message: COULD_NOT_RESIZE.to_string(),
        };
    };
    let Some(resized) = resize::resize_image(
        &normalized.bytes,
        &normalized.mime_type,
        resize_options,
        dims,
        transformer,
    ) else {
        return ImageProcessResult::Failed {
            message: COULD_NOT_RESIZE.to_string(),
        };
    };

    let mut hints = Vec::new();
    if let Some(hint) = conversion_hint(normalized.converted_from.as_deref(), &resized.mime_type) {
        hints.push(hint);
    }
    if let Some(note) = resize::format_dimension_note(&resized) {
        hints.push(note);
    }
    ImageProcessResult::Processed {
        data: resized.data,
        mime_type: resized.mime_type,
        hints,
    }
}

/// Success note in the read tool (`read.ts:287-289`): the MIME type, one line
/// per hint, and optionally the non-vision notice.
#[must_use]
pub fn read_image_note(mime_type: &str, hints: &[String], non_vision: bool) -> String {
    let mut note = format!("Read image file [{mime_type}]");
    for hint in hints {
        note.push('\n');
        note.push_str(hint);
    }
    if non_vision {
        note.push('\n');
        note.push_str(NON_VISION_NOTE);
    }
    note
}

/// Failure note in the read tool (`read.ts:283-285`): the sniffed MIME type,
/// the omission message, and optionally the non-vision notice.
#[must_use]
pub fn read_failure_note(mime_type: &str, message: &str, non_vision: bool) -> String {
    let mut note = format!("Read image file [{mime_type}]\n{message}");
    if non_vision {
        note.push('\n');
        note.push_str(NON_VISION_NOTE);
    }
    note
}

/// Build the image block attached to a read/tool result (`read.ts:292`,
/// `tool-result-images.ts:59`).
#[must_use]
pub fn to_image_content(data: String, mime_type: String) -> ImageContent {
    ImageContent { data, mime_type }
}

/// Strip image blocks when `blockImages` is on (`sdk.ts:296-331`): replace
/// each with a text placeholder and dedupe consecutive placeholders. When the
/// setting is off, the original blocks are returned unchanged.
#[must_use]
pub fn filter_blocked_images(blocks: &[ContentBlock], block_images: bool) -> Vec<ContentBlock> {
    if !block_images
        || !blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
    {
        return blocks.to_vec();
    }
    let placeholder = ContentBlock::Text(TextContent {
        text: IMAGE_READING_DISABLED.to_string(),
        text_signature: None,
    });
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        if matches!(block, ContentBlock::Image(_)) {
            if out.last() == Some(&placeholder) {
                continue;
            }
            out.push(placeholder.clone());
        } else {
            out.push(block.clone());
        }
    }
    out
}

/// Fold a converted message's `TextContent | ImageContent` block back into
/// the `InputContent` shape user/tool-result messages carry. Thinking and
/// tool-call blocks cannot appear there, so they are dropped.
fn input_content_from_block(block: ContentBlock) -> Option<InputContent> {
    match block {
        ContentBlock::Text(text) => Some(InputContent::Text(text)),
        ContentBlock::Image(image) => Some(InputContent::Image(image)),
        ContentBlock::Thinking(_) | ContentBlock::ToolCall(_) => None,
    }
}

/// `sdk.ts:296-331`: the `blockImages` defense-in-depth filter applied to the
/// output of `defaultConvertToLlm`. It runs after the session's custom roles
/// have been folded into user messages, replaces every image block in user
/// and tool-result messages with upstream's "Image reading is disabled."
/// text, and dedupes consecutive placeholders. When `block_images` is false
/// the messages are returned unchanged.
fn filter_blocked_messages(messages: Vec<AgentMessage>, block_images: bool) -> Vec<AgentMessage> {
    if !block_images {
        return messages;
    }
    messages
        .into_iter()
        .map(|message| match message {
            AgentMessage::User(mut user) => {
                if let pi_ai::types::UserContent::Blocks(blocks) = &user.content {
                    let loose: Vec<ContentBlock> =
                        blocks.iter().cloned().map(ContentBlock::from).collect();
                    let filtered = filter_blocked_images(&loose, true);
                    user.content = pi_ai::types::UserContent::Blocks(
                        filtered
                            .into_iter()
                            .filter_map(input_content_from_block)
                            .collect(),
                    );
                }
                AgentMessage::User(user)
            }
            AgentMessage::ToolResult(mut result) => {
                let loose: Vec<ContentBlock> = result
                    .content
                    .iter()
                    .cloned()
                    .map(ContentBlock::from)
                    .collect();
                let filtered = filter_blocked_images(&loose, true);
                result.content = filtered
                    .into_iter()
                    .filter_map(input_content_from_block)
                    .collect();
                AgentMessage::ToolResult(result)
            }
            other => other,
        })
        .collect()
}

/// The `convertToLlm` seam (`sdk.ts:296-331`) with the `blockImages` filter
/// installed over `default_convert_to_llm`. Every headless host (print, JSON,
/// RPC) uses this same builder; the interactive host snapshots the setting
/// per run in its own loop config. Upstream checks the setting on every call;
/// these modes are single-run, so the host snapshots it once at startup.
#[must_use]
pub fn block_images_converter(block_images: bool) -> Arc<dyn pi_agent::ConvertToLlm> {
    Arc::new(move |messages: Vec<AgentMessage>| {
        filter_blocked_messages(pi_agent::default_convert_to_llm(messages), block_images)
    })
}

/// `normalizeToolResultImages` (`tool-result-images.ts:24-67`): normalize
/// arbitrary base64 image blocks as they enter history. On failure the
/// original block is kept, unlike the read tool's omission.
pub fn normalize_tool_result_images(
    content: &[ContentBlock],
    settings: &ImageSettings,
    resize_options: &ResizeOptions,
    transformer: &dyn ImageTransformer,
) -> Vec<ContentBlock> {
    if !content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
    {
        return content.to_vec();
    }
    let mut normalized = Vec::with_capacity(content.len());
    let mut changed = false;
    for block in content {
        let ContentBlock::Image(image) = block else {
            normalized.push(block.clone());
            continue;
        };
        let bytes = BASE64_STANDARD.decode(&image.data).unwrap_or_default();
        let ImageProcessResult::Processed {
            data,
            mime_type,
            hints,
        } = process_image(
            &bytes,
            &image.mime_type,
            settings,
            resize_options,
            transformer,
        )
        else {
            normalized.push(block.clone());
            continue;
        };
        if data == image.data && mime_type == image.mime_type && hints.is_empty() {
            normalized.push(block.clone());
            continue;
        }
        normalized.push(ContentBlock::Image(ImageContent { data, mime_type }));
        if !hints.is_empty() {
            normalized.push(ContentBlock::Text(TextContent {
                text: hints.join("\n"),
                text_signature: None,
            }));
        }
        changed = true;
    }
    if changed {
        normalized
    } else {
        content.to_vec()
    }
}

/// Temp path for an image that cannot be inlined, mirroring the clipboard
/// paste path `os.tmpdir()/pi-clipboard-${uuid}.${ext}`
/// (`interactive-mode.ts:3298-3302`) and `codemode_host.rs::write_output_file`.
#[must_use]
pub fn temp_image_path(mime_type: &str) -> PathBuf {
    let extension = match normalize_supported_image_mime_type(mime_type) {
        Some("image/jpeg") => "jpg",
        Some("image/webp") => "webp",
        Some("image/gif") => "gif",
        _ => "png",
    };
    std::env::temp_dir().join(format!("pi-image-{}.{extension}", uuid::Uuid::new_v4()))
}

/// Spill an oversized original to disk and return its path. `WIRING.md`
/// documents this as a deliberate extension of upstream's "omitted" path.
pub fn write_oversized_temp_file(bytes: &[u8], mime_type: &str) -> Result<String, String> {
    let path = temp_image_path(mime_type);
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

/// Text note pointing the model at a spilled image.
#[must_use]
pub fn oversized_temp_note(mime_type: &str, path: &str) -> String {
    format!("[Oversized image ({mime_type}) saved to {path}.]")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use image::GenericImageView;

    struct PngConverter;

    impl ImageTransformer for PngConverter {
        fn convert_to_png(&self, input: &[u8]) -> Option<Vec<u8>> {
            // Synthetic "converted" payload: a detectable PNG header.
            let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
            png.extend_from_slice(&13_u32.to_be_bytes());
            png.extend_from_slice(b"IHDR");
            png.extend_from_slice(&1_u32.to_be_bytes());
            png.extend_from_slice(&1_u32.to_be_bytes());
            png.extend_from_slice(input);
            Some(png)
        }
    }

    fn tiny_png() -> Vec<u8> {
        let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13_u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2_u32.to_be_bytes());
        png.extend_from_slice(&2_u32.to_be_bytes());
        png.extend_from_slice(&[0_u8; 5]);
        png.extend_from_slice(&[0_u8; 4]);
        png
    }

    #[test]
    fn default_settings_match_upstream_defaults() {
        assert_eq!(
            ImageSettings::default(),
            ImageSettings {
                show_images: true,
                image_width_cells: 60,
                auto_resize: true,
                block_images: false,
            }
        );
        assert_eq!(ImageSettings::clamped_width_cells(0), 1);
        assert_eq!(ImageSettings::clamped_width_cells(80), 80);
    }

    #[test]
    fn mime_normalization_handles_parameters_and_aliases() {
        assert_eq!(base_mime_type("IMAGE/JPG; charset=binary"), "image/jpg");
        assert_eq!(
            normalize_supported_image_mime_type("Image/Jpeg; charset=x"),
            Some("image/jpeg")
        );
        assert_eq!(normalize_supported_image_mime_type("image/bmp"), None);
    }

    #[test]
    fn tiny_png_passes_through_without_resize() {
        let result = process_image(
            &tiny_png(),
            "image/png",
            &ImageSettings::default(),
            &ResizeOptions::default(),
            &UnavailableTransformer,
        );
        assert!(result.is_ok());
        assert_eq!(
            result,
            ImageProcessResult::Processed {
                data: BASE64_STANDARD.encode(tiny_png()),
                mime_type: "image/png".to_string(),
                hints: vec![],
            }
        );
    }

    #[test]
    fn auto_resize_disabled_keeps_raw_bytes() {
        let settings = ImageSettings {
            auto_resize: false,
            ..ImageSettings::default()
        };
        let result = process_image(
            b"raw bytes",
            "image/gif",
            &settings,
            &ResizeOptions::default(),
            &UnavailableTransformer,
        );
        assert_eq!(
            result,
            ImageProcessResult::Processed {
                data: BASE64_STANDARD.encode(b"raw bytes"),
                mime_type: "image/gif".to_string(),
                hints: vec![],
            }
        );
    }

    #[test]
    fn bmp_without_a_converter_is_unconvertible() {
        let result = process_image(
            b"BM whatever",
            "image/bmp",
            &ImageSettings::default(),
            &ResizeOptions::default(),
            &UnavailableTransformer,
        );
        assert_eq!(
            result,
            ImageProcessResult::Failed {
                message: UNCONVERTIBLE_IMAGE.to_string()
            }
        );
    }

    #[test]
    fn oversized_image_without_a_backend_is_omitted() {
        let options = ResizeOptions {
            max_bytes: 8,
            ..ResizeOptions::default()
        };
        let result = process_image(
            &tiny_png(),
            "image/png",
            &ImageSettings::default(),
            &options,
            &UnavailableTransformer,
        );
        assert_eq!(
            result,
            ImageProcessResult::Failed {
                message: COULD_NOT_RESIZE.to_string()
            }
        );
    }

    #[test]
    fn converted_image_adds_a_conversion_hint() {
        let result = process_image(
            b"raw",
            "image/bmp",
            &ImageSettings {
                auto_resize: false,
                ..ImageSettings::default()
            },
            &ResizeOptions::default(),
            &PngConverter,
        );
        assert_eq!(
            result,
            ImageProcessResult::Processed {
                data: BASE64_STANDARD.encode(PngConverter.convert_to_png(b"raw").unwrap()),
                mime_type: "image/png".to_string(),
                hints: vec!["[Image converted from image/bmp to image/png.]".to_string()],
            }
        );
    }

    #[test]
    fn read_notes_include_hints_and_non_vision_notice() {
        let hints = vec!["[a hint]".to_string()];
        assert_eq!(
            read_image_note("image/png", &hints, true),
            format!("Read image file [image/png]\n[a hint]\n{NON_VISION_NOTE}")
        );
        assert_eq!(
            read_failure_note("image/gif", COULD_NOT_RESIZE, false),
            format!("Read image file [image/gif]\n{COULD_NOT_RESIZE}")
        );
    }

    #[test]
    fn image_content_block_uses_the_wire_shape() {
        let block = to_image_content("abc".to_string(), "image/png".to_string());
        // The `#[serde(rename_all = "camelCase")]` on pi-ai's `ImageContent`
        // makes these fields serialize as `type`/`data`/`mimeType`.
        assert_eq!(block.data, "abc");
        assert_eq!(block.mime_type, "image/png");
    }

    #[test]
    fn block_images_filter_replaces_and_dedupes() {
        let image = ContentBlock::Image(ImageContent {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
        });
        let text = ContentBlock::Text(TextContent {
            text: "hello".to_string(),
            text_signature: None,
        });
        let blocks = vec![image.clone(), image.clone(), text.clone()];
        let filtered = filter_blocked_images(&blocks, true);
        assert_eq!(filtered.len(), 2);
        assert_eq!(
            filtered[0],
            ContentBlock::Text(TextContent {
                text: IMAGE_READING_DISABLED.to_string(),
                text_signature: None,
            })
        );
        assert_eq!(filtered[1], text);
        assert_eq!(filter_blocked_images(&blocks, false), blocks);
    }

    /// The installed `convertToLlm` seam itself (the shared builder every
    /// headless mode uses), not just the block-level filter.
    fn image_bearing_messages() -> Vec<AgentMessage> {
        use pi_ai::types::{InputContent, ToolResultMessage, UserContent, UserMessage};
        let image = InputContent::Image(ImageContent {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
        });
        let text = InputContent::Text(TextContent {
            text: "hello".to_string(),
            text_signature: None,
        });
        vec![
            AgentMessage::User(UserMessage {
                content: UserContent::Blocks(vec![image.clone(), image.clone(), text.clone()]),
                timestamp: 1,
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                content: vec![image, text.clone()],
                details: None,
                usage: None,
                nested_calls: None,
                is_error: false,
                timestamp: 2,
                duration_ms: None,
            }),
        ]
    }

    #[test]
    fn block_images_converter_replaces_and_dedupes_when_enabled() {
        use pi_ai::types::UserContent;
        let messages = image_bearing_messages();
        let converted =
            crate::runtime::block_on(block_images_converter(true).call(messages.clone()));
        let AgentMessage::User(user) = &converted[0] else {
            panic!("first message stays a user message");
        };
        let UserContent::Blocks(blocks) = &user.content else {
            panic!("user content stays blocks");
        };
        assert_eq!(blocks.len(), 2, "consecutive placeholders are deduped");
        assert_eq!(blocks[0].as_text(), Some(IMAGE_READING_DISABLED));
        assert_eq!(blocks[1].as_text(), Some("hello"));
        let AgentMessage::ToolResult(result) = &converted[1] else {
            panic!("second message stays a tool result");
        };
        assert_eq!(result.content.len(), 2);
        assert_eq!(result.content[0].as_text(), Some(IMAGE_READING_DISABLED));
        assert_eq!(result.content[1].as_text(), Some("hello"));
    }

    #[test]
    fn block_images_converter_preserves_images_when_disabled() {
        let messages = image_bearing_messages();
        let converted =
            crate::runtime::block_on(block_images_converter(false).call(messages.clone()));
        assert_eq!(converted, messages);
    }

    #[test]
    fn tool_result_normalization_keeps_unconvertible_images() {
        let image = ContentBlock::Image(ImageContent {
            data: "not base64 !!".to_string(),
            mime_type: "image/bmp".to_string(),
        });
        let blocks = vec![image.clone()];
        let result = normalize_tool_result_images(
            &blocks,
            &ImageSettings::default(),
            &ResizeOptions::default(),
            &UnavailableTransformer,
        );
        assert_eq!(result, blocks);
        assert_eq!(
            normalize_tool_result_images(
                &[],
                &ImageSettings::default(),
                &ResizeOptions::default(),
                &UnavailableTransformer
            ),
            Vec::<ContentBlock>::new()
        );
    }

    #[test]
    fn temp_files_use_the_expected_extension() {
        let path = temp_image_path("image/jpeg");
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".jpg")
        );
        assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
        assert!(
            temp_image_path("image/bmp")
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".png")
        );
    }

    // Tiny fixtures encoded by the `image` crate itself: no real user files.
    use image::codecs::bmp::BmpEncoder;
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder, RgbImage};

    fn rgb(width: u32, height: u32) -> RgbImage {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                u8::try_from(x * 37 % 256).unwrap_or_default(),
                u8::try_from(y * 91 % 256).unwrap_or_default(),
                0x40,
            ]);
        }
        image
    }

    fn encode_png(image: &RgbImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode synthetic png");
        bytes
    }

    #[test]
    fn bmp_with_default_backend_produces_a_real_png_attachment() {
        let mut bmp = Vec::new();
        let source = rgb(3, 2);
        BmpEncoder::new(&mut bmp)
            .write_image(
                source.as_raw(),
                source.width(),
                source.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode synthetic bmp");
        let result = process_image(
            &bmp,
            "image/bmp",
            &ImageSettings::default(),
            &ResizeOptions::default(),
            default_transformer(),
        );
        let ImageProcessResult::Processed {
            data,
            mime_type,
            hints,
        } = result
        else {
            panic!("a valid BMP now converts instead of being omitted")
        };
        assert_eq!(mime_type, "image/png");
        assert_eq!(
            hints,
            vec!["[Image converted from image/bmp to image/png.]".to_string()]
        );
        let decoded = image::load_from_memory(&BASE64_STANDARD.decode(&data).expect("base64"))
            .expect("decode converted png");
        assert_eq!(decoded.dimensions(), (3, 2));
    }

    #[test]
    fn oversized_image_with_default_backend_is_resized() {
        let options = ResizeOptions {
            max_width: 10,
            max_height: 10,
            ..ResizeOptions::default()
        };
        let result = process_image(
            &encode_png(&rgb(40, 30)),
            "image/png",
            &ImageSettings::default(),
            &options,
            default_transformer(),
        );
        let ImageProcessResult::Processed { data, hints, .. } = result else {
            panic!("an oversized image now resizes instead of being omitted")
        };
        let decoded = image::load_from_memory(&BASE64_STANDARD.decode(&data).expect("base64"))
            .expect("decode resized candidate");
        assert_eq!(decoded.dimensions(), (10, 8));
        assert!(
            hints
                .iter()
                .any(|hint| hint.contains("[Image: original 40x30, displayed at 10x8.")),
            "{hints:?}"
        );
    }
}

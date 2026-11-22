//! Image resize policy and the pixel-transform seam.
//!
//! Ground truth: TS `packages/coding-agent/src/utils/image-resize-core.ts:1-176`
//! (`DEFAULT_OPTIONS`, `resizeImageInProcess`, `base64Size`) and
//! `utils/image-resize.ts:113-124` (`formatDimensionNote`). The quality steps,
//! two-pass PNG/JPEG preference, and 0.75 dimension shrink loop are ported
//! verbatim; the actual decode/resize/encode is behind [`ImageTransformer`]
//! (Photon/WASM in upstream). The default host backend is
//! [`super::transform::ImageCrateTransformer`]; [`UnavailableTransformer`] is
//! kept as the explicit "no backend" override. See `WIRING.md`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use pi_ai::types::ModelImageResizeOptions;

/// `DEFAULT_OPTIONS.maxWidth` (`image-resize-core.ts:35`).
pub const DEFAULT_MAX_WIDTH: u32 = 2000;
/// `DEFAULT_OPTIONS.maxHeight` (`image-resize-core.ts:36`).
pub const DEFAULT_MAX_HEIGHT: u32 = 2000;
/// `4.5 * 1024 * 1024` (`image-resize-core.ts:32,37`).
pub const DEFAULT_MAX_BYTES: usize = 4_718_592;
/// `DEFAULT_OPTIONS.jpegQuality` (`image-resize-core.ts:38`).
pub const DEFAULT_JPEG_QUALITY: u8 = 80;

/// `ImageResizeOptions` (`image-resize-core.ts:4-9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeOptions {
    pub max_width: u32,
    pub max_height: u32,
    /// Maximum length of the base64 payload in bytes.
    pub max_bytes: usize,
    pub jpeg_quality: u8,
}

impl Default for ResizeOptions {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_MAX_WIDTH,
            max_height: DEFAULT_MAX_HEIGHT,
            max_bytes: DEFAULT_MAX_BYTES,
            jpeg_quality: DEFAULT_JPEG_QUALITY,
        }
    }
}

impl From<ModelImageResizeOptions> for ResizeOptions {
    /// Overlay a model's optional profile on the conservative defaults
    /// (`read.ts:280`, `agent-session.ts:3777`).
    fn from(model: ModelImageResizeOptions) -> Self {
        let defaults = Self::default();
        Self {
            max_width: model
                .max_width
                .and_then(as_u32)
                .unwrap_or(defaults.max_width),
            max_height: model
                .max_height
                .and_then(as_u32)
                .unwrap_or(defaults.max_height),
            max_bytes: model
                .max_bytes
                .and_then(as_usize)
                .unwrap_or(defaults.max_bytes),
            jpeg_quality: model
                .jpeg_quality
                .and_then(as_u8)
                .unwrap_or(defaults.jpeg_quality),
        }
    }
}

fn as_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok()
}

fn as_usize(value: u64) -> Option<usize> {
    usize::try_from(value).ok()
}

fn as_u8(value: u64) -> Option<u8> {
    u8::try_from(value).ok()
}

/// The base64 payload length for `byte_len` raw bytes
/// (`Math.ceil(bytes / 3) * 4`, `image-resize-core.ts:75`).
#[must_use]
pub fn base64_size(byte_len: usize) -> usize {
    byte_len.div_ceil(3) * 4
}

/// Seam for the real pixel work. Upstream loads Photon (Rust/WASM) in a worker
/// thread; the host-side implementation is
/// [`super::transform::ImageCrateTransformer`] (wired as the default by the
/// read tool and `@file` processor). [`UnavailableTransformer`] mirrors Photon
/// being absent and is used by tests that exercise the omission paths.
pub trait ImageTransformer {
    /// Decode `input`, fit it within `target_width`×`target_height`, and encode
    /// it as `mime_type` at `jpeg_quality`. `None` means the backend cannot
    /// perform this conversion.
    fn transform(
        &self,
        _input: &[u8],
        _target_width: u32,
        _target_height: u32,
        _mime_type: &str,
        _jpeg_quality: u8,
    ) -> Option<EncodedImage> {
        None
    }

    /// Re-encode arbitrary bytes as PNG (Photon's `convertImageBytesToPng`,
    /// `image-convert.ts:23-30`). Used for formats pi cannot inline directly.
    fn convert_to_png(&self, _input: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

/// One encoded candidate from a transformer (`image-resize-core.ts:41-54`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedImage {
    pub data: Vec<u8>,
    pub mime_type: String,
}

/// A backend that cannot decode or encode images: the explicit "Photon is
/// unavailable" override, not the default any more (see `WIRING.md`).
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableTransformer;

impl ImageTransformer for UnavailableTransformer {}

/// The successful result of `resizeImage` (`image-resize-core.ts:11-19`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizedImage {
    pub data: String,
    pub mime_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

/// `formatDimensionNote` (`image-resize.ts:117-124`): give the model the
/// coordinate mapping when an image was resized.
#[must_use]
pub fn format_dimension_note(result: &ResizedImage) -> Option<String> {
    if !result.was_resized {
        return None;
    }
    let scale = f64::from(result.original_width) / f64::from(result.width);
    Some(format!(
        "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {scale:.2} to map to original image.]",
        result.original_width, result.original_height, result.width, result.height
    ))
}

/// `inputBase64Size < maxBytes` plus the dimension caps
/// (`image-resize-core.ts:92-103`): can the original be inlined unchanged?
#[must_use]
pub fn within_limits(dimensions: (u32, u32), byte_len: usize, options: &ResizeOptions) -> bool {
    dimensions.0 <= options.max_width
        && dimensions.1 <= options.max_height
        && base64_size(byte_len) < options.max_bytes
}

/// First-pass target dimensions (`image-resize-core.ts:105-116`): scale on
/// width first, then height, using JavaScript's `Math.round` (half up).
#[must_use]
pub fn target_dimensions(original: (u32, u32), options: &ResizeOptions) -> (u32, u32) {
    let (mut width, mut height) = original;
    if width > options.max_width {
        height = round_ratio(height, options.max_width, width);
        width = options.max_width;
    }
    if height > options.max_height {
        width = round_ratio(width, options.max_height, height);
        height = options.max_height;
    }
    (width, height)
}

fn round_ratio(value: u32, numerator: u32, denominator: u32) -> u32 {
    let denominator = u64::from(denominator);
    let rounded = (u64::from(value) * u64::from(numerator) + denominator / 2) / denominator;
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

/// `Array.from(new Set([opts.jpegQuality, 85, 70, 55, 40]))`
/// (`image-resize-core.ts:132`): configured quality first, then fallbacks,
/// with duplicates removed while preserving order.
#[must_use]
pub fn quality_steps(quality: u8) -> Vec<u8> {
    let mut steps = Vec::with_capacity(5);
    for candidate in [quality, 85, 70, 55, 40] {
        if !steps.contains(&candidate) {
            steps.push(candidate);
        }
    }
    steps
}

/// Port of `resizeImageInProcess` (`image-resize-core.ts:69-176`). `dims` are
/// the natural dimensions supplied by the caller (Photon in upstream, header
/// sniffing here). `None` when no candidate fits below `max_bytes`.
pub fn resize_image(
    input: &[u8],
    mime_type: &str,
    options: &ResizeOptions,
    dims: (u32, u32),
    transformer: &dyn ImageTransformer,
) -> Option<ResizedImage> {
    if within_limits(dims, input.len(), options) {
        return Some(ResizedImage {
            data: BASE64_STANDARD.encode(input),
            mime_type: mime_type.to_string(),
            original_width: dims.0,
            original_height: dims.1,
            width: dims.0,
            height: dims.1,
            was_resized: false,
        });
    }

    let (mut width, mut height) = target_dimensions(dims, options);
    loop {
        let candidates = try_encodings(transformer, input, width, height, options);
        for candidate in preferred_candidates(&candidates) {
            if base64_size(candidate.data.len()) < options.max_bytes {
                return Some(ResizedImage {
                    data: BASE64_STANDARD.encode(&candidate.data),
                    mime_type: candidate.mime_type.clone(),
                    original_width: dims.0,
                    original_height: dims.1,
                    width,
                    height,
                    was_resized: true,
                });
            }
        }
        if width == 1 && height == 1 {
            break;
        }
        let (next_width, next_height) = shrink(width, height);
        if (next_width, next_height) == (width, height) {
            break;
        }
        width = next_width;
        height = next_height;
    }
    None
}

/// `tryEncodings` (`image-resize-core.ts:118-130`): PNG first, then every
/// quality step as JPEG.
fn try_encodings(
    transformer: &dyn ImageTransformer,
    input: &[u8],
    width: u32,
    height: u32,
    options: &ResizeOptions,
) -> Vec<EncodedImage> {
    let mut candidates = Vec::with_capacity(1 + 5);
    if let Some(png) =
        transformer.transform(input, width, height, "image/png", options.jpeg_quality)
    {
        candidates.push(png);
    }
    for quality in quality_steps(options.jpeg_quality) {
        if let Some(jpeg) = transformer.transform(input, width, height, "image/jpeg", quality) {
            candidates.push(jpeg);
        }
    }
    candidates
}

/// `preferred` plus the remaining lower-quality JPEGs
/// (`image-resize-core.ts:138-141`): prefer whichever of PNG and the
/// configured-quality JPEG is smaller.
fn preferred_candidates(candidates: &[EncodedImage]) -> Vec<&EncodedImage> {
    let mut ordered = Vec::with_capacity(candidates.len());
    let png = candidates.first();
    let jpeg = candidates.get(1);
    match (png, jpeg) {
        (Some(png), Some(jpeg)) if base64_size(jpeg.data.len()) < base64_size(png.data.len()) => {
            ordered.extend([jpeg, png]);
        }
        (Some(png), Some(jpeg)) => ordered.extend([png, jpeg]),
        (Some(png), None) => ordered.push(png),
        (None, Some(jpeg)) => ordered.push(jpeg),
        (None, None) => {}
    }
    ordered.extend(candidates.iter().skip(2));
    ordered
}

/// `Math.max(1, Math.floor(dim * 0.75))` (`image-resize-core.ts:158-159`),
/// expressed in integers to avoid float drift.
fn shrink(width: u32, height: u32) -> (u32, u32) {
    let next = |dim: u32| -> u32 {
        if dim == 1 {
            1
        } else {
            u32::try_from(u64::from(dim) * 3 / 4)
                .unwrap_or(u32::MAX)
                .max(1)
        }
    };
    (next(width), next(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_size_uses_ceiling_division() {
        assert_eq!(base64_size(0), 0);
        assert_eq!(base64_size(1), 4);
        assert_eq!(base64_size(3), 4);
        assert_eq!(base64_size(4), 8);
    }

    #[test]
    fn target_dimensions_port_upstream_rounding() {
        let options = ResizeOptions::default();
        assert_eq!(target_dimensions((4000, 3000), &options), (2000, 1500));
        assert_eq!(target_dimensions((1000, 4000), &options), (500, 2000));
        assert_eq!(target_dimensions((120, 80), &options), (120, 80));
    }

    #[test]
    fn within_limits_checks_all_three_caps() {
        let options = ResizeOptions {
            max_bytes: 8,
            ..ResizeOptions::default()
        };
        // base64 of 6 bytes is exactly 8, so it is NOT strictly below max_bytes.
        assert!(within_limits((10, 10), 3, &options));
        assert!(!within_limits((10, 10), 6, &options));
        assert!(!within_limits((2001, 1), 1, &options));
        assert!(!within_limits((1, 2001), 1, &options));
        assert!(!within_limits((1, 1), 7, &options));
    }

    #[test]
    fn quality_steps_dedupe_preserving_order() {
        assert_eq!(quality_steps(80), vec![80, 85, 70, 55, 40]);
        assert_eq!(quality_steps(85), vec![85, 70, 55, 40]);
        assert_eq!(quality_steps(40), vec![40, 85, 70, 55]);
    }

    #[test]
    fn shrink_floors_to_at_least_one() {
        assert_eq!(shrink(2000, 1500), (1500, 1125));
        assert_eq!(shrink(2, 3), (1, 2));
        assert_eq!(shrink(1, 1), (1, 1));
    }

    #[test]
    fn dimension_note_is_only_for_resized_images() {
        let mut result = ResizedImage {
            data: String::new(),
            mime_type: "image/png".to_string(),
            original_width: 4000,
            original_height: 3000,
            width: 2000,
            height: 1500,
            was_resized: false,
        };
        assert_eq!(format_dimension_note(&result), None);
        result.was_resized = true;
        assert_eq!(
            format_dimension_note(&result).as_deref(),
            Some(
                "[Image: original 4000x3000, displayed at 2000x1500. Multiply coordinates by 2.00 to map to original image.]"
            )
        );
    }

    #[test]
    fn within_limits_returns_original_bytes_unchanged() {
        let transformer = UnavailableTransformer;
        let result = resize_image(
            b"tiny",
            "image/png",
            &ResizeOptions::default(),
            (2, 2),
            &transformer,
        )
        .expect("within limits");
        assert!(!result.was_resized);
        assert_eq!(result.mime_type, "image/png");
        assert_eq!(result.data, BASE64_STANDARD.encode(b"tiny"));
    }

    /// Output size scales with width, so only a later shrink pass fits the
    /// byte budget. This exercises the 0.75 loop rather than the first pass.
    struct ShrinkingTransformer;

    impl ImageTransformer for ShrinkingTransformer {
        fn transform(
            &self,
            _input: &[u8],
            width: u32,
            _height: u32,
            mime_type: &str,
            _quality: u8,
        ) -> Option<EncodedImage> {
            Some(EncodedImage {
                data: vec![0_u8; usize::try_from(width).unwrap_or(usize::MAX) / 100],
                mime_type: mime_type.to_string(),
            })
        }
    }

    #[test]
    fn oversized_image_uses_shrink_loop_and_reports_new_dims() {
        let options = ResizeOptions {
            // 2000px -> 20 bytes (base64 28, too big); 1500px -> 15 bytes
            // (base64 20, fits). The first pass must therefore be rejected.
            max_bytes: 24,
            ..ResizeOptions::default()
        };
        let result = resize_image(
            &[0_u8; 16],
            "image/png",
            &options,
            (4000, 3000),
            &ShrinkingTransformer,
        )
        .expect("a candidate fits after shrinking");
        assert!(result.was_resized);
        assert_eq!((result.width, result.height), (1500, 1125));
        assert_eq!(result.data.len(), 20);
    }

    #[test]
    fn no_backend_means_oversized_images_cannot_be_resized() {
        let options = ResizeOptions {
            max_bytes: 8,
            ..ResizeOptions::default()
        };
        let result = resize_image(
            &[0_u8; 100],
            "image/png",
            &options,
            (4000, 3000),
            &UnavailableTransformer,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn model_resize_options_overlay_defaults() {
        let options = ResizeOptions::from(ModelImageResizeOptions {
            max_width: Some(1024),
            max_height: Some(768),
            max_bytes: Some(1000),
            jpeg_quality: Some(50),
        });
        assert_eq!(
            options,
            ResizeOptions {
                max_width: 1024,
                max_height: 768,
                max_bytes: 1000,
                jpeg_quality: 50
            }
        );
        let defaults = ResizeOptions::from(ModelImageResizeOptions::default());
        assert_eq!(defaults, ResizeOptions::default());
    }
}

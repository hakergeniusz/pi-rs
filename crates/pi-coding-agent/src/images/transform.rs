//! The real pixel backend: decode → Lanczos3 resize → PNG/JPEG encode.
//!
//! Ground truth: TS `packages/coding-agent/src/utils/image-resize-core.ts:69-176`
//! (`resizeImageInProcess`: Photon `new_from_byteslice`, `SamplingFilter.Lanczos3`,
//! `get_bytes`/`get_bytes_jpeg`, and one decode per resize attempt) and
//! `utils/image-convert.ts:7-30` (`encodePng` / `convertImageBytesToPng`). The
//! byte budget, quality steps, PNG-vs-JPEG preference and 0.75 shrink loop are
//! owned by [`super::resize`]; this module supplies only the decode/encode seam
//! upstream delegates to Photon (Rust/WASM).
//!
//! `image`'s `FilterType::Lanczos3` is the host equivalent of Photon's
//! `SamplingFilter.Lanczos3`, so the sampling filter stays 1:1. The remaining
//! mechanism-level differences (no EXIF orientation pass, no worker thread,
//! re-decoding per resize attempt) are catalogued in `WIRING.md`.

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, load_from_memory};

use super::resize::{EncodedImage, ImageTransformer};

/// Host replacement for upstream's Photon/WASM backend, using the offline
/// cached pure-Rust `image` crate (png/jpeg/gif/webp/bmp decoders, png/jpeg
/// encoders only — see the workspace dependency entry).
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageCrateTransformer;

impl ImageCrateTransformer {
    /// `photon.PhotonImage.new_from_byteslice` (`image-resize-core.ts:84`):
    /// sniff the format from the bytes and decode. `None` on any decode error.
    fn decode(input: &[u8]) -> Option<DynamicImage> {
        load_from_memory(input).ok()
    }

    /// Photon's `get_bytes()` / `get_bytes_jpeg(quality)`
    /// (`image-resize-core.ts:122-125`). Photon images are always RGBA8, so PNG
    /// output is RGBA8; JPEG drops alpha because it has no alpha channel.
    fn encode(image: &DynamicImage, mime_type: &str, jpeg_quality: u8) -> Option<EncodedImage> {
        let mut data = Vec::new();
        match mime_type {
            "image/png" => {
                let rgba = image.to_rgba8();
                PngEncoder::new(&mut data)
                    .write_image(
                        rgba.as_raw(),
                        rgba.width(),
                        rgba.height(),
                        ExtendedColorType::Rgba8,
                    )
                    .ok()?;
            }
            "image/jpeg" => {
                let rgb = image.to_rgb8();
                JpegEncoder::new_with_quality(&mut data, jpeg_quality)
                    .write_image(
                        rgb.as_raw(),
                        rgb.width(),
                        rgb.height(),
                        ExtendedColorType::Rgb8,
                    )
                    .ok()?;
            }
            // The resize policy only ever asks for PNG or JPEG.
            _ => return None,
        }
        Some(EncodedImage {
            data,
            mime_type: mime_type.to_string(),
        })
    }
}

impl ImageTransformer for ImageCrateTransformer {
    /// `photon.resize(image, width, height, SamplingFilter.Lanczos3)`
    /// (`image-resize-core.ts:119`) followed by the two encodings above.
    fn transform(
        &self,
        input: &[u8],
        target_width: u32,
        target_height: u32,
        mime_type: &str,
        jpeg_quality: u8,
    ) -> Option<EncodedImage> {
        // Upstream's `Math.round` target dimensions can round a very thin
        // image's other axis to 0; Photon throws and the caller returns null.
        if target_width == 0 || target_height == 0 {
            return None;
        }
        let decoded = Self::decode(input)?;
        let resized = decoded.resize_exact(target_width, target_height, FilterType::Lanczos3);
        Self::encode(&resized, mime_type, jpeg_quality)
    }

    /// `convertImageBytesToPng` (`image-convert.ts:23-30`): decode any supported
    /// format (notably BMP) and re-encode as PNG for inline attachment.
    fn convert_to_png(&self, input: &[u8]) -> Option<Vec<u8>> {
        let decoded = Self::decode(input)?;
        let rgba = decoded.to_rgba8();
        let mut data = Vec::new();
        PngEncoder::new(&mut data)
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                ExtendedColorType::Rgba8,
            )
            .ok()?;
        Some(data)
    }
}

/// The canonical default transformer passed to [`super::process_image`] by the
/// read tool and the `@file` processor. A unit struct needs no allocation, so
/// callers share a single `&'static` trait object.
#[must_use]
pub fn default_transformer() -> &'static dyn ImageTransformer {
    &ImageCrateTransformer
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use image::GenericImageView;
    use super::*;

    use image::RgbImage;
    use image::codecs::bmp::BmpEncoder;

    fn rgb(width: u32, height: u32) -> RgbImage {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                u8::try_from(x * 37 % 256).unwrap_or_default(),
                u8::try_from(y * 91 % 256).unwrap_or_default(),
                0x80,
            ]);
        }
        image
    }

    fn png_bytes(image: &RgbImage) -> Vec<u8> {
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

    fn bmp_bytes(image: &RgbImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        BmpEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .expect("encode synthetic bmp");
        bytes
    }

    #[test]
    fn resize_and_encode_as_png() {
        let input = png_bytes(&rgb(8, 6));
        let candidate = ImageCrateTransformer
            .transform(&input, 4, 3, "image/png", 80)
            .expect("png candidate");
        assert_eq!(candidate.mime_type, "image/png");
        let decoded = load_from_memory(&candidate.data).expect("decode png candidate");
        assert_eq!((decoded.width(), decoded.height()), (4, 3));
    }

    #[test]
    fn resize_and_encode_as_jpeg_at_the_requested_quality() {
        let input = png_bytes(&rgb(8, 8));
        let high = ImageCrateTransformer
            .transform(&input, 4, 4, "image/jpeg", 95)
            .expect("jpeg candidate");
        let low = ImageCrateTransformer
            .transform(&input, 4, 4, "image/jpeg", 40)
            .expect("jpeg candidate");
        assert_eq!(high.mime_type, "image/jpeg");
        let high_decoded = load_from_memory(&high.data).expect("decode");
        assert_eq!((high_decoded.width(), high_decoded.height()), (4, 4));
        assert_eq!(low.mime_type, "image/jpeg");
        let low_decoded = load_from_memory(&low.data).expect("decode");
        assert_eq!((low_decoded.width(), low_decoded.height()), (4, 4));
        assert!(
            low.data.len() < high.data.len(),
            "quality 40 should be smaller than quality 95"
        );
    }

    #[test]
    fn convert_to_png_decodes_a_valid_bmp() {
        let bmp = bmp_bytes(&rgb(3, 2));
        let png = ImageCrateTransformer
            .convert_to_png(&bmp)
            .expect("bmp converts to png");
        let decoded = load_from_memory(&png).expect("decode converted png");
        assert_eq!(decoded.dimensions(), (3, 2));
    }

    #[test]
    fn invalid_bytes_and_zero_dimensions_fail_closed() {
        assert_eq!(
            ImageCrateTransformer.transform(b"not an image", 4, 4, "image/png", 80),
            None
        );
        assert_eq!(ImageCrateTransformer.convert_to_png(b"not an image"), None);
        let input = png_bytes(&rgb(4, 4));
        assert_eq!(
            ImageCrateTransformer.transform(&input, 0, 4, "image/png", 80),
            None
        );
    }

    #[test]
    fn default_transformer_performs_real_pixel_work() {
        let bmp = bmp_bytes(&rgb(2, 2));
        let png = default_transformer()
            .convert_to_png(&bmp)
            .expect("bmp converts through the default seam");
        assert_eq!(load_from_memory(&png).expect("decode").dimensions(), (2, 2));
    }
}

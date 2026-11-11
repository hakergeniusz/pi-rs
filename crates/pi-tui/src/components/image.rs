//! `Image`: an inline image, or the textual fallback. `packages/tui/src/components/image.ts`.
//!
//! The component owns the two decisions pi makes in `render`: how many cells
//! the image may take at this width, and what to emit when the terminal has no
//! image protocol. Everything else — the transmission itself, the Kitty image
//! id, the metadata registry — stays in
//! [`crate::terminal_image`].

use std::cell::{Cell, RefCell};

use crate::component::Component;
use crate::terminal_image::{
    ImageDimensions, ImageProtocol, get_capabilities, get_cell_dimensions, get_image_dimensions,
    image_fallback, render_image,
};

/// Colour hook for the fallback text. `ImageTheme`, `image.ts:11-13`.
pub struct ImageTheme {
    pub fallback_color: Box<dyn Fn(&str) -> String + Send>,
}

impl ImageTheme {
    /// A theme that leaves the fallback text alone.
    pub fn plain() -> Self {
        ImageTheme {
            fallback_color: Box::new(|text: &str| text.to_string()),
        }
    }
}

impl std::fmt::Debug for ImageTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImageTheme")
    }
}

/// `ImageOptions`, `image.ts:15-21`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub filename: Option<String>,
    /// Kitty image id. Reusing one id re-transmits into the same slot, which
    /// is what pi's animations rely on.
    pub image_id: Option<u32>,
}

/// `Image`, `image.ts:23-127`.
#[derive(Debug)]
pub struct Image {
    base64_data: String,
    mime_type: String,
    dimensions: ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    /// Assigned on the first Kitty render, then remembered.
    assigned_image_id: Cell<Option<u32>>,
    cached_lines: RefCell<Option<(usize, Vec<String>)>>,
}

impl Image {
    /// `image.ts:29-43`. Without `dimensions` the size is sniffed out of the
    /// data, and pi's 800x600 guess stands in when that fails.
    pub fn new(
        base64_data: String,
        mime_type: String,
        theme: ImageTheme,
        options: ImageOptions,
        dimensions: Option<ImageDimensions>,
    ) -> Self {
        let dimensions = dimensions
            .or_else(|| get_image_dimensions(&base64_data, &mime_type))
            .unwrap_or(ImageDimensions {
                width_px: 800,
                height_px: 600,
            });
        let image_id = Cell::new(options.image_id);
        Image {
            base64_data,
            mime_type,
            dimensions,
            theme,
            options,
            assigned_image_id: image_id,
            cached_lines: RefCell::new(None),
        }
    }

    /// The Kitty image id this component transmits under, once it has
    /// rendered. `getImageId`, `image.ts:46-48`.
    pub fn image_id(&self) -> Option<u32> {
        self.assigned_image_id.get()
    }

    /// The pixel size pi settled on.
    pub fn dimensions(&self) -> ImageDimensions {
        self.dimensions
    }

    /// `image.ts:71-74`.
    fn fallback_line(&self, width: usize) -> String {
        let fallback = image_fallback(
            &self.mime_type,
            Some(&self.dimensions),
            self.options.filename.as_deref(),
        );
        let coloured = (self.theme.fallback_color)(&fallback);
        crate::ansi::truncate_to_width(&coloured, width, "...", false)
    }
}

impl Component for Image {
    /// `image.ts:64-125`.
    fn render(&self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = self.cached_lines.borrow().as_ref() {
            if *cached_width == width {
                return lines.clone();
            }
        }

        // `Math.max(1, Math.min(width - 2, maxWidthCells ?? 60))`. The
        // two-cell margin is pi's; `width - 2` underflows at 0 and 1 in Rust,
        // and pi's `Math.max(1, …)` is what clamps it.
        let max_width = width
            .saturating_sub(2)
            .min(self.options.max_width_cells.unwrap_or(60))
            .max(1);
        let cell_dimensions = get_cell_dimensions();
        let default_max_height = (max_width as f64 * cell_dimensions.width_px as f64
            / cell_dimensions.height_px as f64)
            .ceil() as usize;
        let max_height = self
            .options
            .max_height_cells
            .unwrap_or(default_max_height.max(1));

        let capabilities = get_capabilities();
        let lines = if capabilities.images == ImageProtocol::Disabled {
            vec![self.fallback_line(width)]
        } else {
            // `:73-75`: Kitty needs an id before the transmission, and every
            // protocol draws with `C=1` so the cursor never moves.
            if capabilities.images == ImageProtocol::Kitty && self.assigned_image_id.get().is_none()
            {
                self.assigned_image_id
                    .set(Some(crate::terminal_image::allocate_image_id()));
            }
            match render_image(
                &self.base64_data,
                &self.dimensions,
                &crate::terminal_image::ImageRenderOptions {
                    max_width_cells: Some(max_width),
                    max_height_cells: Some(max_height),
                    preserve_aspect_ratio: None,
                    image_id: self.assigned_image_id.get(),
                    move_cursor: Some(false),
                },
            ) {
                Some(result) => {
                    if let Some(image_id) = result.image_id {
                        self.assigned_image_id.set(Some(image_id));
                    }
                    if capabilities.images == ImageProtocol::Kitty {
                        // `:89-96`: the sequence owns its row; the rest of the
                        // image's rows are blank so the layout reserves them.
                        let mut lines = vec![result.sequence.clone()];
                        lines.extend(std::iter::repeat_n(
                            String::new(),
                            result.rows.saturating_sub(1),
                        ));
                        lines
                    } else {
                        // `:98-106`: iTerm2 draws at the cursor, so the rows
                        // above are blank and the last line moves up first.
                        let mut lines = vec![String::new(); result.rows.saturating_sub(1)];
                        let row_offset = result.rows.saturating_sub(1);
                        let move_up = if row_offset > 0 {
                            format!("\x1b[{row_offset}A")
                        } else {
                            String::new()
                        };
                        lines.push(format!("{move_up}{}", result.sequence));
                        lines
                    }
                }
                None => vec![self.fallback_line(width)],
            }
        };

        *self.cached_lines.borrow_mut() = Some((width, lines.clone()));
        lines
    }

    /// `image.ts:50-53`.
    fn invalidate(&mut self) {
        *self.cached_lines.borrow_mut() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_image::{
        CellDimensions, TerminalCapabilities, reset_capabilities_cache, reset_kitty_image_metadata,
        set_capabilities, set_cell_dimensions,
    };

    fn image(options: ImageOptions, dimensions: ImageDimensions) -> Image {
        Image::new(
            "AAAA".to_string(),
            "image/png".to_string(),
            ImageTheme::plain(),
            options,
            Some(dimensions),
        )
    }

    fn caps(images: ImageProtocol) -> TerminalCapabilities {
        TerminalCapabilities {
            images,
            true_color: true,
            hyperlinks: true,
        }
    }

    fn cleanup() {
        reset_kitty_image_metadata();
        reset_capabilities_cache();
        set_capabilities(caps(ImageProtocol::Disabled));
        set_cell_dimensions(CellDimensions {
            width_px: 9,
            height_px: 18,
        });
    }

    #[test]
    fn ac1_no_protocol_renders_the_fallback_line() {
        cleanup();
        let component = image(
            ImageOptions {
                filename: Some("shot.png".to_string()),
                ..Default::default()
            },
            ImageDimensions {
                width_px: 1280,
                height_px: 720,
            },
        );
        assert_eq!(
            component.render(40),
            vec!["[Image: shot.png [image/png] 1280x720]"]
        );
    }

    #[test]
    fn ac2_kitty_reserves_one_line_per_image_row() {
        cleanup();
        set_capabilities(caps(ImageProtocol::Kitty));
        set_cell_dimensions(CellDimensions {
            width_px: 10,
            height_px: 10,
        });
        // pi's default height is the aspect ratio; two rows is what this test
        // wants to look at.
        let component = image(
            ImageOptions {
                max_height_cells: Some(2),
                ..Default::default()
            },
            ImageDimensions {
                width_px: 100,
                height_px: 100,
            },
        );
        let lines = component.render(20);
        assert_eq!(lines.len(), 2, "two rows of cells: {lines:?}");
        assert!(
            lines[0].contains("\x1b_Ga=T,"),
            "the transmission comes first: {lines:?}"
        );
        assert!(
            lines[0].contains("C=1"),
            "and it never moves the cursor: {lines:?}"
        );
        assert_eq!(lines[1], "", "the second row is reserved, not drawn");
        assert!(
            component.image_id().is_some(),
            "Kitty render assigns an image id"
        );
    }

    #[test]
    fn ac3_iterm2_moves_the_cursor_up_before_the_image() {
        cleanup();
        set_capabilities(caps(ImageProtocol::Iterm2));
        set_cell_dimensions(CellDimensions {
            width_px: 10,
            height_px: 10,
        });
        let component = image(
            ImageOptions {
                max_height_cells: Some(2),
                ..Default::default()
            },
            ImageDimensions {
                width_px: 100,
                height_px: 100,
            },
        );
        let lines = component.render(20);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "", "the rows above the image are blank");
        assert!(
            lines[1].starts_with("\x1b[1A\x1b]1337;File="),
            "the last line moves up then draws: {lines:?}"
        );
    }

    #[test]
    fn ac4_the_max_width_is_the_option_capped_by_the_viewport() {
        cleanup();
        set_capabilities(caps(ImageProtocol::Kitty));
        set_cell_dimensions(CellDimensions {
            width_px: 10,
            height_px: 10,
        });
        // `width - 2` wins over the option, and the option wins over 60.
        let narrow = image(
            ImageOptions {
                max_width_cells: Some(4),
                ..Default::default()
            },
            ImageDimensions {
                width_px: 400,
                height_px: 400,
            },
        );
        assert!(
            narrow.render(6)[0].contains(",c=4,"),
            "the viewport margin caps it: {:?}",
            narrow.render(6)
        );
        let wide = image(
            ImageOptions {
                max_width_cells: Some(4),
                ..Default::default()
            },
            ImageDimensions {
                width_px: 400,
                height_px: 400,
            },
        );
        assert!(
            wide.render(80)[0].contains(",c=4,"),
            "the option caps it too: {:?}",
            wide.render(80)
        );
    }

    #[test]
    fn ac5_an_explicit_image_id_is_reused() {
        cleanup();
        set_capabilities(caps(ImageProtocol::Kitty));
        set_cell_dimensions(CellDimensions {
            width_px: 10,
            height_px: 10,
        });
        let component = image(
            ImageOptions {
                image_id: Some(321),
                ..Default::default()
            },
            ImageDimensions {
                width_px: 100,
                height_px: 100,
            },
        );
        assert!(
            component.render(20)[0].contains("i=321"),
            "the id from the options is used"
        );
        assert_eq!(component.image_id(), Some(321));
    }

    #[test]
    fn ac6_invalidate_drops_the_cached_lines() {
        cleanup();
        set_capabilities(caps(ImageProtocol::Kitty));
        set_cell_dimensions(CellDimensions {
            width_px: 10,
            height_px: 10,
        });
        let mut component = image(
            ImageOptions::default(),
            ImageDimensions {
                width_px: 100,
                height_px: 100,
            },
        );
        let first = component.render(20);
        assert_eq!(component.render(20), first, "the same width hits the cache");
        component.invalidate();
        assert_eq!(
            component.render(20),
            first,
            "and the rebuild is byte-identical"
        );
    }

    #[test]
    fn ac7_the_fallback_is_truncated_to_the_width() {
        cleanup();
        let component = image(
            ImageOptions {
                filename: Some("a-very-long-image-filename-indeed.png".to_string()),
                ..Default::default()
            },
            ImageDimensions {
                width_px: 10,
                height_px: 10,
            },
        );
        let line = &component.render(20)[0];
        assert_eq!(
            crate::ansi::visible_width(line),
            20,
            "truncated to the viewport: {line:?}"
        );
        assert!(
            line.ends_with("\x1b[0m...\x1b[0m"),
            "pi closes the style around the ellipsis: {line:?}"
        );
    }
}

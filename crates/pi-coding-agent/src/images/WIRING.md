# Image subsystem — host wiring notes

This directory is a port of pi 1.5.2's image attachment/input pipeline. The
**primary host wiring has landed** (`e8308bb`): `read.rs`, `file_processor.rs`
and the interactive `convertToLlm` path call into this module. The notes below
record the original recipe and the remaining tails; see `docs/GAP-STATUS.md`
for current coverage.

## Files added

| File | Contents |
|---|---|
| `mod.rs` | `ImageSettings`, `process_image`, `ImageProcessResult`, read-note builders, `ImageContent` construction, `filter_blocked_images` (`blockImages`), `normalize_tool_result_images`, temp-file spill helpers. |
| `sniff.rs` | Magic-byte detection for jpeg/png/gif/webp/bmp (port of `utils/mime.ts:1-116`) plus header dimension readers. |
| `resize.rs` | `ResizeOptions` (+ `From<ModelImageResizeOptions>`), the 2000×2000 / 4.5 MB resize policy ported from `utils/image-resize-core.ts`, `formatDimensionNote`, and the `ImageTransformer` seam. |

Add one line to `crates/pi-coding-agent/src/lib.rs` (the only host edit needed
to compile the module):

```rust
pub mod images;
```

## Host call sites (edit these, not this directory)

**Status:** `read.rs`, `file_processor.rs` and the interactive `convertToLlm`
filter are **wired** (`e8308bb`); `lib.rs` registers `pub mod images`. Still
open: the print/JSON/RPC `convertToLlm` paths, clipboard-image paste/temp files,
live `ShowImagesChange` refresh, and `normalize_tool_result_images` on tool/MCP
image blocks. The recipe below is retained for those tails.

### `crates/pi-coding-agent/src/tools/read.rs`

- `execute_image` at `:186-230` currently calls a reduced local `process_image`
  (`:244-256`) and builds the note inline (`:194`, `:205-209`).
- Replace the local `ProcessedImage` enum / `process_image` with
  `pi_coding_agent::images::process_image(&bytes, mime_type, &ImageSettings::from_settings(...), &ResizeOptions::from(model_profile.unwrap_or_default()), &transformer)`.
- Replace the inline notes with `images::read_image_note(...)` and
  `images::read_failure_note(...)`; build the block with
  `images::to_image_content(data, mime_type)` instead of the literal at `:215-218`.
- Keep the `structuredContent` shape at `:220-225` unchanged.
- Add `ctx.model.input` / `ctx.model.input_limits.images.resize` as the
  `non_vision` and `ResizeOptions` inputs (TS `read.ts:274-281`). The current
  signature of `execute_image` has no context, so thread the settings/model
  snapshot through from `execute` at `:93-94`.
- The duplicate MIME sniffer at `:562-694` can delegate to
  `images::sniff::detect_supported_image_format` (same semantics, incl.
  lossless-JPEG and animated-PNG rejection); leaving it is harmless.

### `crates/pi-coding-agent/src/cli/file_processor.rs`

- `process_file_arguments` at `:45-100`, image branch `:68-88`: it currently
  inlines raw bytes for the four provider formats and emits the
  could-not-convert text for BMP (`:69-79`). Route the branch through
  `images::process_image` with `auto_resize: false` (the print arm passes
  `autoResizeImages: false`, TS `main.ts:270`). BMP then correctly takes the
  conversion-failure path when no transformer is installed.
- The private `detect_supported_image_mime` at `:109-135` is byte-for-byte the
  same rule as `images::sniff::detect_supported_image_format`; delegation is
  optional.

### `crates/pi-coding-agent/src/modes/interactive/interactive_mode.rs`

- `:5153-5156` already snapshots the four settings into the settings selector.
  Use `ImageSettings::from_settings(self.settings)` when building the tool
  registry / `read` tool options (TS `agent-session.ts:3766-3779`).
- `:5227-5243` persists the four setting changes. The upstream live-sweep of
  `ToolExecutionComponent.setShowImages` is still pending infrastructure;
  after a `ShowImagesChange`, rebuild the tool options from the fresh
  `ImageSettings` (TS `interactive-mode.ts:5153-5160`).
- `show_images` / `image_width_cells` are **display-only**: pass them to
  `get_text_output(content, show_images)` in
  `crates/pi-coding-agent/src/tools/render_utils.rs:83-109` and to pi-tui's
  `components/image.rs::ImageOptions` (`width_cells`). Do not feed them into
  `process_image`.
- `block_images` is **not** applied at image creation: it is a
  `convertToLlm`-time filter. Call `images::filter_blocked_images(&blocks,
  settings.block_images)` where the port builds provider messages from
  `ContentBlock`s (TS `sdk.ts:296-331`, `interactive_mode.rs` call sites at
  `:3161`, `:3409`, `:8733`, `:9546`).
- Call `images::normalize_tool_result_images` once as extension/MCP/tool image
  blocks enter session history (TS `tool-result-images.ts:24-67`).

### Clipboard paste / temp files

No clipboard-image read exists in the port yet. When it lands, the image paste
at TS `interactive-mode.ts:3296-3305` needs `images::temp_image_path` (already
mirrors `os.tmpdir()/pi-clipboard-<uuid>.<ext>`); `write_oversized_temp_file` +
`oversized_temp_note` are the same mechanism for images that cannot be
inlined.

## Dependencies

**No new dependencies are required for this directory.** It uses only crates
already in `crates/pi-coding-agent/Cargo.toml`: `base64`, `uuid`, `pi-ai`, and
`tempfile` (dev). Do **not** edit `Cargo.toml` / `Cargo.lock` for this slice.

If the host wants a real pixel backend, implement `ImageTransformer`:

```rust
impl images::ImageTransformer for MyImageCrateBackend {
    fn transform(&self, input: &[u8], w: u32, h: u32, mime: &str, q: u8)
        -> Option<images::resize::EncodedImage> { /* decode -> resize -> encode */ }
    fn convert_to_png(&self, input: &[u8]) -> Option<Vec<u8>> { /* re-encode */ }
}
```

The cached pure-Rust `image` crate (`0.24.9` / `0.25.10` are in the offline
registry) is sufficient for decode/resize/encode; adding it to the workspace
is a **separate, host-owned** change because this slice may not touch
`Cargo.toml`. With the default `UnavailableTransformer`, oversized images take
upstream's "could not be resized / could not be converted" omission paths, and
images already within limits pass through byte-for-byte — matching upstream
when Photon is unavailable.

## Deviations from upstream (deliberate, mechanism-level)

1. **Dimensions from headers, not a decode.** Photon reports `originalWidth`/
   `originalHeight` after decoding; `sniff::image_dimensions` reads them from
   PNG `IHDR`, JPEG SOF, GIF logical screen, WebP `VP8 `/`VP8L`/`VP8X`, and BMP
   DIB headers. Header dimensions are trusted for the resize decision; a
   truncated/implausible header yields the "could not be resized" note.
2. **Transform is a seam, not Photon.** The decode/Lanczos3-resize/re-encode
   step is `ImageTransformer`; the default is a no-op that reports
   "unavailable", exactly like upstream when Photon fails to load. Quality
   steps, PNG-vs-JPEG preference, and the 0.75 shrink loop are ported verbatim.
3. **Header-only "sniff bytes".** Upstream reads 4100 bytes from disk; callers
   here already hold the full bytes. Detection logic is identical.
4. **Temp-file spill for oversized images.** Upstream always omits an image it
   cannot inline. `write_oversized_temp_file` / `oversized_temp_note` add an
   optional path that preserves the bytes on disk for the model to read with
   `bash`/`read`; callers that want 1:1 upstream behaviour simply keep using
   `process_image`'s `Failed` variant.
5. **Terminal rendering stays in pi-tui.** No new renderer is introduced;
   `terminal_image.rs` (Kitty/iTerm2 encode + cell sizing) and
   `components/image.rs` remain the display path. `show_images` and
   `image_width_cells` only gate/label that path, as upstream.
6. **BMP sniffing is kept but not inlined.** `image/bmp` is detected, then
   takes the conversion-to-PNG route; without a transformer that is upstream's
   "could not be converted" note (this is also what the existing print path
   does at `file_processor.rs:69-79`).

## Tests

29 unit tests, all in `#[cfg(test)] mod tests` at the end of each file:
`sniff.rs` 7 (jpeg/png/gif/webp/bmp sniffing, lossless JPEG and animated PNG
rejection, header dimensions), `resize.rs` 10 (base64 sizing, target dims,
byte/dimension gating, quality steps, shrink loop, model overlay, format note),
`mod.rs` 12 (settings defaults, MIME normalization, pass-through, auto-resize
off, conversion hints, omission notes, content shape, blocked filter, tool
result normalization, temp paths).

Verified with `rustfmt --edition 2024` and a standalone `rustc`/`clippy-driver`
harness using the workspace's pedantic lint set (the coordinator runs the real
cargo gate after wiring).

## Ground-truth citations

- `utils/mime.ts:1-116` — `sniff.rs` detection.
- `utils/image-process.ts:1-119` — `mod.rs` normalization + omission messages.
- `utils/image-resize-core.ts:1-176`, `utils/image-resize.ts:113-124` —
  `resize.rs` policy and note.
- `core/tools/read.ts:27-38,95-107,275-294` — read notes / image block.
- `utils/tool-result-images.ts:1-67` — history normalization.
- `core/sdk.ts:296-331`, `core/settings-defaults.ts:35-44` — settings semantics.
- `utils/clipboard-image.ts:26-43`, `modes/interactive/interactive-mode.ts:3296-3305`
  — temp-file paths.

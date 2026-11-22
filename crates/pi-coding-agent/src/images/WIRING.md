# Image subsystem — host wiring notes

This directory is a port of pi 1.5.2's image attachment/input pipeline. The
**primary host wiring has landed** (`e8308bb`): `read.rs`, `file_processor.rs`
and the interactive `convertToLlm` path call into this module. The notes below
record the original recipe and the remaining tails; see `docs/GAP-STATUS.md`
for current coverage.

## Files added

| File | Contents |
|---|---|
| `mod.rs` | `ImageSettings`, `process_image`, `ImageProcessResult`, read-note builders, `ImageContent` construction, `filter_blocked_images` (`blockImages`), `block_images_converter` (the shared `convertToLlm` seam), `normalize_tool_result_images`, temp-file spill helpers. |
| `sniff.rs` | Magic-byte detection for jpeg/png/gif/webp/bmp (port of `utils/mime.ts:1-116`) plus header dimension readers. |
| `resize.rs` | `ResizeOptions` (+ `From<ModelImageResizeOptions>`), the 2000×2000 / 4.5 MB resize policy ported from `utils/image-resize-core.ts`, `formatDimensionNote`, and the `ImageTransformer` seam. |
| `transform.rs` | `ImageCrateTransformer` — the real decode / Lanczos3 resize / PNG+JPEG encode backend and PNG conversion (`convert_to_png`), plus `default_transformer()`. Backed by the offline-cached `image` crate; replaces Photon/WASM. |

Add one line to `crates/pi-coding-agent/src/lib.rs` (the only host edit needed
to compile the module):

```rust
pub mod images;
```

## Host call sites (edit these, not this directory)

**Status:** `read.rs`, `file_processor.rs` and the interactive `convertToLlm`
filter are **wired** (`e8308bb`); `lib.rs` registers `pub mod images`. Both
image-producing call sites now pass `default_transformer()` instead of
`UnavailableTransformer`, so oversized images are resized and BMP is converted
to PNG. Still open: clipboard-image paste/temp files, live `ShowImagesChange`
refresh, and `normalize_tool_result_images` on tool/MCP image blocks (that seam
still takes an explicit transformer from its caller). The recipe below is
retained for those tails.

### `crates/pi-coding-agent/src/tools/read.rs` — done

- `execute_image` calls `images::process_image(&bytes, mime_type,
  &ImageSettings::default(), &ResizeOptions::default(),
  images::default_transformer())` and builds the note/block with
  `images::read_image_note` / `images::read_failure_note` /
  `images::to_image_content`; the `structuredContent` shape is unchanged.
- Still a deliberate deviation: the builtin `ToolContext` carries no model
  metadata, so `getNonVisionImageNote(ctx.model)` (`read.ts:274`) takes its
  no-model arm and the conservative fallback resize options apply. Threading a
  model snapshot through `execute` would close that gap.
- The MIME sniffer now delegates to `images::detect_supported_image_format`
  (same semantics, incl. lossless-JPEG and animated-PNG rejection).

### `crates/pi-coding-agent/src/cli/file_processor.rs` — done

- The image branch of `process_file_arguments` calls `images::process_image`
  with `auto_resize: false` (the print arm passes `autoResizeImages: false`,
  TS `main.ts:270`) and `default_transformer()`. Supported formats attach
  byte-for-byte; a valid BMP now attaches as PNG with the conversion hint.
  Malformed BMPs still take the could-not-convert text path.
- Detection is shared with the read tool via
  `images::detect_supported_image_format`.

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
  `:3161`, `:3409`, `:8733`, `:9546`). The interactive host snapshots
  `settings.get_block_images()` per run and wraps
  `default_convert_to_llm` in its own message-level filter.

### `crates/pi-coding-agent/src/modes/{print_mode,json_event,rpc}.rs` + `main.rs`

- All three headless modes now install `images::block_images_converter(block_images)`
  as `AgentLoopConfig::convert_to_llm` instead of `DefaultConvertToLlm`. The
  builder runs `default_convert_to_llm` first, then the same
  `filter_blocked_images` logic on user/tool-result messages, so the setting
  is a defense-in-depth filter over the converted transcript (`sdk.ts:296-331`).
- The host snapshots `settings.get_block_images()` once in
  `build_runtime_with_ui` (next to `retry_settings` / `compaction_settings`)
  and threads the bool through `run_print_mode`, `run_json_mode`,
  `run_rpc_mode` → `handle_command` → `run_agent_turn`. This mirrors the
  interactive per-run snapshot; upstream re-reads the setting on every call.
- Tests: enabled/disabled cases at the block level (`mod.rs`), at the installed
  seam (`mod.rs`, print/JSON tests), and end-to-end through an RPC prompt with
  a capturing stream (`rpc.rs`).
- Call `images::normalize_tool_result_images` once as extension/MCP/tool image
  blocks enter session history (TS `tool-result-images.ts:24-67`).

### Clipboard paste / temp files

No clipboard-image read exists in the port yet. When it lands, the image paste
at TS `interactive-mode.ts:3296-3305` needs `images::temp_image_path` (already
mirrors `os.tmpdir()/pi-clipboard-<uuid>.<ext>`); `write_oversized_temp_file` +
`oversized_temp_note` are the same mechanism for images that cannot be
inlined.

## Dependencies

The real pixel backend adds the offline-cached pure-Rust **`image` crate
0.25.10** to the workspace (`default-features = false`, codecs `png`, `jpeg`,
`gif`, `webp`, `bmp` only — decoders for every format pi inlines plus BMP for
conversion, encoders only for png/jpeg). `pi-coding-agent` depends on it via
the workspace table; the registry-resident transitive deps (`png`, `gif`,
`image-webp`, `zune-jpeg`, `moxcms`, …) are already cached, so `cargo
check --offline` resolves without network access. The rest of the directory
still uses only `base64`, `uuid`, `pi-ai` and `tempfile` (dev).

Hosts that want to override the backend can still implement `ImageTransformer`
and pass it to `process_image`; `UnavailableTransformer` remains the explicit
"Photon is absent" override used by the omission-path tests:

```rust
impl images::ImageTransformer for MyBackend {
    fn transform(&self, input: &[u8], w: u32, h: u32, mime: &str, q: u8)
        -> Option<images::resize::EncodedImage> { /* decode -> resize -> encode */ }
    fn convert_to_png(&self, input: &[u8]) -> Option<Vec<u8>> { /* re-encode */ }
}
```

The canonical default is `images::default_transformer()`, wired into the read
tool and the `@file` processor. It decodes with `image::load_from_memory`,
resizes with `FilterType::Lanczos3` (upstream's `SamplingFilter.Lanczos3`),
encodes PNG (RGBA8) and JPEG (RGB, configured quality), and re-encodes BMP to
PNG. Images already within limits still pass through byte-for-byte, exactly
like upstream's early return (`image-resize-core.ts:92-103`).

## Deviations from upstream (deliberate, mechanism-level)

1. **Dimensions from headers, not a decode.** Photon reports `originalWidth`/
   `originalHeight` after decoding; `sniff::image_dimensions` reads them from
   PNG `IHDR`, JPEG SOF, GIF logical screen, WebP `VP8 `/`VP8L`/`VP8X`, and BMP
   DIB headers. Header dimensions are trusted for the resize decision; a
   truncated/implausible header yields the "could not be resized" note.
2. **Transform is `image`, not Photon.** The `ImageTransformer` seam is now
   implemented by `ImageCrateTransformer` (the default via
   `default_transformer()`): `image::load_from_memory` decodes,
   `FilterType::Lanczos3` resizes (same filter as Photon), and png/jpeg encoders
   produce the candidates. Quality steps, PNG-vs-JPEG preference, and the 0.75
   shrink loop in `resize.rs` are ported verbatim. `UnavailableTransformer`
   remains only as the explicit no-backend override. Minor mechanism deltas:
   Photon applies EXIF orientation before decoding (`exif-orientation.ts`,
   `image-resize-core.ts:85`) which `image` does not; Photon decodes once per
   resize request while this backend decodes on each `transform` call; and the
   upstream worker thread is unnecessary because this path is synchronous.
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
6. **BMP is converted, not omitted.** `image/bmp` is detected and the default
   transformer's `convert_to_png` decodes it and emits a real PNG attachment,
   with the same "[Image converted from image/bmp to image/png.]" hint
   upstream produces when Photon is present. A malformed/header-only BMP still
   fails conversion and takes the omission path. Animated PNG/JPEG-LS are still
   rejected by the sniffer before any decode, as upstream does.

## Tests

36 unit tests in this directory, all in `#[cfg(test)] mod tests` at the end of
each file: `sniff.rs` 7 (jpeg/png/gif/webp/bmp sniffing, lossless JPEG and
animated PNG rejection, header dimensions), `resize.rs` 10 (base64 sizing,
target dims, byte/dimension gating, quality steps, shrink loop, model overlay,
format note), `mod.rs` 14 (settings defaults, MIME normalization, pass-through,
auto-resize off, conversion hints, omission notes, content shape, blocked
filter, tool result normalization, temp paths, plus real-backend BMP conversion
and oversize resize), `transform.rs` 5 (PNG/JPEG resize+encode, quality ordering,
BMP→PNG, fail-closed on bad bytes, default seam). Three more end-to-end tests
live in the wired call sites: `read.rs` 2 (valid BMP attachment, oversize resize
attachment) and `file_processor.rs` 1 (valid BMP attachment). All fixtures are
synthetic images produced by the `image` crate — no real user files.

Verified with `cargo fmt` and `cargo test -p pi-coding-agent images::` plus the
wired call-site tests (the coordinator runs the full workspace gate).

## Ground-truth citations

- `utils/mime.ts:1-116` — `sniff.rs` detection.
- `utils/image-process.ts:1-119` — `mod.rs` normalization + omission messages.
- `utils/image-resize-core.ts:1-176`, `utils/image-resize.ts:113-124` —
  `resize.rs` policy and note; `transform.rs` ports the decode/Lanczos3/encode
  seam (`:82-130`), while EXIF orientation (`:85`, `utils/exif-orientation.ts`)
  is a documented deviation.
- `utils/image-convert.ts:7-30` — `transform.rs::convert_to_png`.
- `core/tools/read.ts:27-38,95-107,275-294` — read notes / image block.
- `utils/tool-result-images.ts:1-67` — history normalization.
- `core/sdk.ts:296-331`, `core/settings-defaults.ts:35-44` — settings semantics.
- `utils/clipboard-image.ts:26-43`, `modes/interactive/interactive-mode.ts:3296-3305`
  — temp-file paths.

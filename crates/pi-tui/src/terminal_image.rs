//! Terminal capabilities, Kitty / iTerm2 inline image encoding, and OSC 8.
//!
//! Ground truth: `packages/tui/src/terminal-image.ts` (732) and
//! `packages/tui/test/terminal-image.test.ts` (845). Every item below cites
//! the pi line range it mirrors.
//!
//! Two seams keep this module hermetic:
//!
//! * [`TerminalEnv`] carries the environment as `&str` values instead of
//!   reading the process environment. Hosts build it with
//!   [`TerminalEnv::from_process_env`]; tests build it with
//!   [`TerminalEnv::set`], so no test ever sees the developer's `$TERM`.
//! * The tmux `client_termfeatures` probe is a caller-supplied closure
//!   ([`TmuxProbe`], [`detect_capabilities`]). No code path here spawns
//!   `tmux`: only [`probe_tmux_hyperlinks`], which the host may leave
//!   uninstalled, shells out, and it is never reached from tests because
//!   [`detect_capabilities`] takes the probe as an argument.
//!
//! Module state ([`get_capabilities`], cell dimensions, the Kitty metadata
//! registry) is `thread_local!`, matching pi's module-level `let` bindings
//! while keeping concurrent tests isolated from each other.
//!
//! Base64 encoding/decoding is inline (`base64_encode`, `base64_decode`); the
//! crate takes no new dependency for it.

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::rc::Rc;

use crate::color::TerminalColorMode;

/// Kitty graphics protocol introducer. `terminal-image.ts:198`.
pub const KITTY_PREFIX: &str = "\x1b_G";
/// iTerm2 inline image introducer. `terminal-image.ts:199`.
pub const ITERM2_PREFIX: &str = "\x1b]1337;File=";
/// Base64 characters per Kitty transmission chunk. `terminal-image.ts:221`.
pub const CHUNK_SIZE: usize = 4096;
/// Mandatory Kitty transmit-and-display params. `terminal-image.ts:223`.
pub const KITTY_BASE_PARAMS: [&str; 3] = ["a=T", "f=100", "q=2"];
/// Registry cap before the oldest entry is evicted. `terminal-image.ts:346`.
pub const MAX_KITTY_METADATA_ENTRIES: usize = 1000;
/// Bytes-per-pixel estimate for `estimatedDecodedBytes`. `terminal-image.ts:418`.
pub const ESTIMATED_DECODED_BYTES_PER_PIXEL: u64 = 4;
/// Controls replayed by a placement-only command. `terminal-image.ts:371-390`.
pub const KITTY_PLACEMENT_CONTROL_KEYS: [&str; 17] = [
    "i", "p", "x", "y", "w", "h", "X", "Y", "c", "r", "C", "U", "z", "P", "Q", "H", "V",
];
/// `renderImage` width ceiling when the caller gives none. `terminal-image.ts:644`.
pub const RENDER_DEFAULT_MAX_WIDTH_CELLS: usize = 80;
/// `Image` component width ceiling when the caller gives none. `components/image.ts`.
pub const IMAGE_COMPONENT_DEFAULT_MAX_WIDTH_CELLS: usize = 60;
/// Probe command pi shells out to. `terminal-image.ts:56`.
pub const TMUX_PROBE_CMD: &str = "tmux display-message -p '#{client_termfeatures}'";
/// pi's probe deadline. `terminal-image.ts:57`.
///
/// This crate owns no timer; the deadline is advisory for hosts. See
/// [`probe_tmux_hyperlinks`].
pub const TMUX_PROBE_TIMEOUT_MS: u64 = 250;

/// Inline image protocol in use. `terminal-image.ts:7`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageProtocol {
    /// Kitty graphics protocol.
    Kitty,
    /// iTerm2 inline images.
    Iterm2,
    /// pi's `images: null`: no inline images.
    #[default]
    Disabled,
}

/// Detected/overridden terminal features. `terminal-image.ts:9-13`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalCapabilities {
    pub images: ImageProtocol,
    pub true_color: bool,
    pub hyperlinks: bool,
}

/// Cell size in pixels, from the `CSI 16 t` reply. `terminal-image.ts:15-18`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for CellDimensions {
    /// pi's default cell is `{ widthPx: 9, heightPx: 18 }`. `terminal-image.ts:39`.
    fn default() -> Self {
        Self {
            width_px: 9,
            height_px: 18,
        }
    }
}

/// Pixel dimensions of an encoded image. `terminal-image.ts:20-23`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

/// Cell footprint chosen for an image. `terminal-image.ts:449-451`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellSize {
    pub columns: usize,
    pub rows: usize,
}

/// `renderImage` options. `terminal-image.ts:25-33`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRenderOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub preserve_aspect_ratio: Option<bool>,
    pub image_id: Option<u32>,
    pub move_cursor: Option<bool>,
}

/// Kitty per-image bookkeeping. `terminal-image.ts:314-318`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyImageMetadata {
    pub image_id: u32,
    pub columns: usize,
    pub rows: usize,
    pub width_px: u32,
    pub height_px: u32,
}

/// Registry entry plus its transmission generation. `terminal-image.ts:320-328`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredKittyImageMetadata {
    pub metadata: KittyImageMetadata,
    pub transmission_generation: u64,
}

/// Placement-only rewrite of a Kitty image line. `terminal-image.ts:330-339`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImagePlacement {
    pub image_id: u32,
    pub transmission_generation: u64,
    pub transmission_bytes: usize,
    pub estimated_decoded_bytes: u64,
    pub sequence: String,
    pub replacement_line: String,
}

/// `renderImage` result. `terminal-image.ts:639`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderImageResult {
    pub sequence: String,
    pub columns: usize,
    pub rows: usize,
    pub image_id: Option<u32>,
}

/// Kitty transmit-and-display options. `terminal-image.ts:221-232`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KittyEncodeOptions {
    pub columns: Option<usize>,
    pub rows: Option<usize>,
    pub image_id: Option<u32>,
    pub move_cursor: Option<bool>,
}

/// iTerm2 inline-image options; `width`/`height` are strings so `"auto"`
/// passes through. `terminal-image.ts:289-294`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ITerm2EncodeOptions {
    pub width: Option<String>,
    pub height: Option<String>,
    pub name: Option<String>,
    pub preserve_aspect_ratio: Option<bool>,
    pub inline: Option<bool>,
}

/// Partial override set; `images` is `Option<Option<..>>` so "override to
/// disabled" is distinguishable from "no override". `terminal-image.ts:181`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PartialTerminalCapabilities {
    pub images: Option<Option<ImageProtocol>>,
    pub true_color: Option<bool>,
    pub hyperlinks: Option<bool>,
}

// ---------------------------------------------------------------------------
// Injectable environment
// ---------------------------------------------------------------------------

/// Environment snapshot, keyed by the variable names pi reads.
///
/// Empty values are treated as unset, matching JavaScript truthiness in
/// `terminal-image.ts:77-113` (where `process.env.TMUX` is falsy when `""`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalEnv {
    vars: Vec<(String, String)>,
    /// Stand-in for `process.platform === "win32"`. `terminal-image.ts:77`.
    pub is_windows: bool,
}

impl TerminalEnv {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the variables pi consults plus the platform check.
    pub fn from_process_env() -> Self {
        const NAMES: [&str; 15] = [
            "TERM",
            "TERM_PROGRAM",
            "TERMINAL_EMULATOR",
            "COLORTERM",
            "TMUX",
            "KITTY_WINDOW_ID",
            "GHOSTTY_RESOURCES_DIR",
            "WEZTERM_PANE",
            "WARP_SESSION_ID",
            "WARP_TERMINAL_SESSION_UUID",
            "ITERM_SESSION_ID",
            "WT_SESSION",
            "PI_IMAGE_PROTOCOL",
            "PI_TRUE_COLOR",
            "PI_HYPERLINKS",
        ];
        let mut env = Self::new();
        for name in NAMES {
            if let Ok(value) = std::env::var(name) {
                env.set(name, &value);
            }
        }
        env.is_windows = cfg!(windows);
        env
    }

    pub fn set(&mut self, name: &str, value: &str) -> &mut Self {
        match self.vars.iter_mut().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = value.to_string(),
            None => self.vars.push((name.to_string(), value.to_string())),
        }
        self
    }

    pub fn unset(&mut self, name: &str) -> &mut Self {
        self.vars.retain(|(key, _)| key != name);
        self
    }

    /// `None` for missing and empty values.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    }

    fn lower(&self, name: &str) -> String {
        self.get(name).unwrap_or_default().to_lowercase()
    }

    fn is_windows_console(&self) -> bool {
        self.is_windows
    }
}

/// tmux probe seam: returns the raw `client_termfeatures` string, or `None`
/// when tmux is absent or errors. `terminal-image.ts:54-68`.
pub type TmuxProbe = Box<dyn Fn() -> Option<String>>;

/// Shell out to tmux for `client_termfeatures`. `terminal-image.ts:56-61`.
///
/// pi passes a 250 ms `timeout`; this crate owns no timer, so a host that
/// needs the deadline must wrap the call. Any error yields `None`, which
/// [`detect_capabilities`] reads as "no hyperlinks".
pub fn probe_tmux_hyperlinks() -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Module state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Registry {
    /// Insertion-ordered so the oldest entry is `entries[0]`. `terminal-image.ts:343`.
    entries: Vec<RegisteredKittyImageMetadata>,
    generation: u64,
}

thread_local! {
    static CAPABILITIES_CACHE: RefCell<Option<TerminalCapabilities>> = const { RefCell::new(None) };
    static CAPABILITY_OVERRIDES: RefCell<PartialTerminalCapabilities> =
        const { RefCell::new(PartialTerminalCapabilities { images: None, true_color: None, hyperlinks: None }) };
    static CAPABILITY_ENV: RefCell<Option<TerminalEnv>> = const { RefCell::new(None) };
    static CAPABILITY_PROBE: RefCell<Option<RcProbe>> = const { RefCell::new(None) };
    static CELL_DIMENSIONS: RefCell<CellDimensions> =
        const { RefCell::new(CellDimensions { width_px: 9, height_px: 18 }) };
    static KITTY_REGISTRY: RefCell<Registry> = const { RefCell::new(Registry { entries: Vec::new(), generation: 0 }) };
}

/// Stored form of [`TmuxProbe`]; `Rc` so `get_capabilities` can hand the same
/// closure to the detector without consuming the registration.
type RcProbe = Rc<dyn Fn() -> Option<String>>;

/// Replace the environment used by [`get_capabilities`]. `None` restores the
/// real process environment.
pub fn set_capability_environment(env: Option<TerminalEnv>) {
    CAPABILITY_ENV.with(|cell| *cell.borrow_mut() = env);
    reset_capabilities_cache();
}

/// Replace the tmux probe used by [`get_capabilities`].
pub fn set_tmux_probe(probe: Option<TmuxProbe>) {
    CAPABILITY_PROBE.with(|cell| *cell.borrow_mut() = probe.map(Rc::from));
    reset_capabilities_cache();
}

/// Current cell dimensions. `terminal-image.ts:41-43`.
pub fn get_cell_dimensions() -> CellDimensions {
    CELL_DIMENSIONS.with(|cell| *cell.borrow())
}

/// Replace cell dimensions wholesale; no invalidation happens here, matching
/// pi. `terminal-image.ts:45-47`.
pub fn set_cell_dimensions(dimensions: CellDimensions) {
    CELL_DIMENSIONS.with(|cell| *cell.borrow_mut() = dimensions);
}

/// Clear the cached capabilities. `terminal-image.ts:176-178`.
pub fn reset_capabilities_cache() {
    CAPABILITIES_CACHE.with(|cell| *cell.borrow_mut() = None);
}

/// Override selected detected capabilities; unchanged sets are a no-op.
/// `terminal-image.ts:180-192`.
pub fn set_capability_overrides(overrides: PartialTerminalCapabilities) {
    CAPABILITY_OVERRIDES.with(|cell| {
        let mut current = cell.borrow_mut();
        if *current == overrides {
            return;
        }
        *current = overrides;
    });
    reset_capabilities_cache();
}

/// Force the cached capabilities. `terminal-image.ts:194-196`.
pub fn set_capabilities(capabilities: TerminalCapabilities) {
    CAPABILITIES_CACHE.with(|cell| *cell.borrow_mut() = Some(capabilities));
}

/// Environment seen by [`get_capabilities`].
pub fn capability_environment() -> TerminalEnv {
    CAPABILITY_ENV
        .with(|cell| cell.borrow().clone())
        .unwrap_or_else(TerminalEnv::from_process_env)
}

/// Detect capabilities for `env`, using `probe` only when the tmux branch is
/// reached and `PI_HYPERLINKS` did not already decide. `terminal-image.ts:140-159`.
pub fn detect_capabilities(
    env: &TerminalEnv,
    probe: &dyn Fn() -> Option<String>,
) -> TerminalCapabilities {
    let hyperlinks_override = parse_boolean_capability_override(env.get("PI_HYPERLINKS"));
    let detected = detect_capabilities_from_environment(env, &|| match hyperlinks_override {
        Some(value) => Some(if value {
            "hyperlinks".to_string()
        } else {
            String::new()
        }),
        None => probe(),
    });

    let protocol = env.lower("PI_IMAGE_PROTOCOL");
    let images = match protocol.as_str() {
        "kitty" => Some(ImageProtocol::Kitty),
        "iterm2" => Some(ImageProtocol::Iterm2),
        "none" | "0" => Some(ImageProtocol::Disabled),
        _ => None,
    };
    let true_color = parse_boolean_capability_override(env.get("PI_TRUE_COLOR"));

    TerminalCapabilities {
        images: images.unwrap_or(detected.images),
        true_color: true_color.unwrap_or(detected.true_color),
        hyperlinks: hyperlinks_override.unwrap_or(detected.hyperlinks),
    }
}

/// The env truth table. `terminal-image.ts:70-134`.
pub fn detect_capabilities_from_environment(
    env: &TerminalEnv,
    tmux_forwards_hyperlink: &dyn Fn() -> Option<String>,
) -> TerminalCapabilities {
    let term_program = env.lower("TERM_PROGRAM");
    let terminal_emulator = env.lower("TERMINAL_EMULATOR");
    let term = env.lower("TERM");
    let color_term = env.lower("COLORTERM");
    let has_true_color_hint =
        color_term == "truecolor" || color_term == "24bit" || term.ends_with("-direct");

    let caps = |images, true_color, hyperlinks: bool| TerminalCapabilities {
        images,
        true_color,
        hyperlinks,
    };

    // tmux strips image protocols unless passthrough is configured, so pi
    // leaves images off and gates OSC 8 on `client_termfeatures`.
    if env.get("TMUX").is_some() || term.starts_with("tmux") {
        return caps(
            ImageProtocol::Disabled,
            has_true_color_hint,
            tmux_forwards_hyperlink().is_some_and(|features| {
                features
                    .split(',')
                    .map(str::trim)
                    .any(|feature| feature == "hyperlinks")
            }),
        );
    }

    if term.starts_with("screen") {
        return caps(ImageProtocol::Disabled, has_true_color_hint, false);
    }
    if env.get("KITTY_WINDOW_ID").is_some() || term_program == "kitty" {
        return caps(ImageProtocol::Kitty, true, true);
    }
    if term_program == "ghostty"
        || term.contains("ghostty")
        || env.get("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return caps(ImageProtocol::Kitty, true, true);
    }
    if env.get("WEZTERM_PANE").is_some() || term_program == "wezterm" {
        return caps(ImageProtocol::Kitty, true, true);
    }
    if term_program == "warpterminal"
        || env.get("WARP_SESSION_ID").is_some()
        || env.get("WARP_TERMINAL_SESSION_UUID").is_some()
    {
        return caps(ImageProtocol::Kitty, true, true);
    }
    if env.get("ITERM_SESSION_ID").is_some() || term_program == "iterm.app" {
        return caps(ImageProtocol::Iterm2, true, true);
    }
    if env.get("WT_SESSION").is_some() {
        return caps(ImageProtocol::Disabled, true, true);
    }
    if term_program == "alacritty" || term_program == "vscode" || term_program == "zed" {
        return caps(ImageProtocol::Disabled, true, true);
    }
    if terminal_emulator == "jetbrains-jediterm" {
        return caps(ImageProtocol::Disabled, true, false);
    }
    if env.is_windows_console() {
        return caps(ImageProtocol::Disabled, true, false);
    }
    // Unknown terminal: OSC 8 would vanish on terminals that swallow it, so
    // stay on the legacy plain-text path.
    caps(ImageProtocol::Disabled, has_true_color_hint, false)
}

/// `"1"` → true, `"0"` → false, anything else → no override.
/// `terminal-image.ts:136-138`.
pub fn parse_boolean_capability_override(value: Option<&str>) -> Option<bool> {
    match value {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

/// Cached capabilities: detect, then merge overrides. `terminal-image.ts:161-170`.
pub fn get_capabilities() -> TerminalCapabilities {
    if let Some(cached) = CAPABILITIES_CACHE.with(|cell| *cell.borrow()) {
        return cached;
    }
    let overrides = CAPABILITY_OVERRIDES.with(|cell| *cell.borrow());
    let env = capability_environment();
    let probe = CAPABILITY_PROBE.with(|cell| cell.borrow().clone());
    // A `hyperlinks` override replaces the probe result wholesale, which is
    // what pi's closure argument does at `terminal-image.ts:168`.
    let probe_fn: RcProbe = match overrides.hyperlinks {
        Some(value) => Rc::from(Box::new(move || {
            Some(if value {
                "hyperlinks".to_string()
            } else {
                String::new()
            })
        }) as Box<dyn Fn() -> Option<String>>),
        None => match probe {
            Some(probe) => probe,
            None => Rc::new(probe_tmux_hyperlinks),
        },
    };
    let probe_closure: &dyn Fn() -> Option<String> = &*probe_fn;
    let layered = detect_capabilities(&env, probe_closure);
    let merged = TerminalCapabilities {
        images: overrides.images.flatten().unwrap_or(layered.images),
        true_color: overrides.true_color.unwrap_or(layered.true_color),
        hyperlinks: overrides.hyperlinks.unwrap_or(layered.hyperlinks),
    };
    set_capabilities(merged);
    merged
}

/// `"truecolor"` / `"256color"`. `terminal-image.ts:172-174`.
pub fn get_terminal_color_mode(capabilities: &TerminalCapabilities) -> TerminalColorMode {
    if capabilities.true_color {
        TerminalColorMode::TrueColor
    } else {
        TerminalColorMode::Color256
    }
}

// ---------------------------------------------------------------------------
// Line classification and ids
// ---------------------------------------------------------------------------

/// True when the line carries an inline image, anywhere. `terminal-image.ts:201-212`.
pub fn is_image_line(line: &str) -> bool {
    line.starts_with(KITTY_PREFIX)
        || line.starts_with(ITERM2_PREFIX)
        || line.contains(KITTY_PREFIX)
        || line.contains(ITERM2_PREFIX)
}

/// Allocate an image id in `[1, 0xfffffffe]`. `terminal-image.ts:215-217`.
///
/// pi uses `floor(random() * 0xfffffffe) + 1`; the seed is drawn from std's
/// `RandomState` so no `rand` dependency and no clock are needed.
pub fn allocate_image_id() -> u32 {
    allocate_image_id_from_seed(RandomState::new().build_hasher().finish())
}

/// Deterministic half of [`allocate_image_id`], so the range is testable.
pub fn allocate_image_id_from_seed(seed: u64) -> u32 {
    // `floor(random * 0xfffffffe)` for a 64-bit random is `seed % 0xfffffffe`.
    ((seed % 0xffff_fffe) + 1) as u32
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Kitty transmit-and-display sequence, chunked at 4096 base64 characters.
/// `terminal-image.ts:220-267`.
pub fn encode_kitty(base64_data: &str, options: KittyEncodeOptions) -> String {
    let mut params: Vec<String> = KITTY_BASE_PARAMS.iter().map(|p| (*p).to_string()).collect();
    if options.move_cursor == Some(false) {
        params.push("C=1".to_string());
    }
    if let Some(columns) = options.columns.filter(|value| *value != 0) {
        params.push(format!("c={columns}"));
    }
    if let Some(rows) = options.rows.filter(|value| *value != 0) {
        params.push(format!("r={rows}"));
    }
    if let Some(image_id) = options.image_id.filter(|value| *value != 0) {
        params.push(format!("i={image_id}"));
    }

    if base64_data.len() <= CHUNK_SIZE {
        return format!("{KITTY_PREFIX}{};{base64_data}\x1b\\", params.join(","));
    }

    let mut chunks: Vec<String> = Vec::new();
    let data: Vec<char> = base64_data.chars().collect();
    let len = data.len();
    let mut offset = 0usize;
    loop {
        let end = (offset + CHUNK_SIZE).min(len);
        let chunk: String = data[offset..end].iter().collect();
        let is_last = offset + CHUNK_SIZE >= len;
        if offset == 0 {
            chunks.push(format!(
                "{KITTY_PREFIX}{},m=1;{chunk}\x1b\\",
                params.join(",")
            ));
        } else if is_last {
            chunks.push(format!("{KITTY_PREFIX}m=0;{chunk}\x1b\\"));
        } else {
            chunks.push(format!("{KITTY_PREFIX}m=1;{chunk}\x1b\\"));
        }
        if is_last {
            break;
        }
        offset += CHUNK_SIZE;
    }
    chunks.concat()
}

/// Delete one transmitted image and its placements. `terminal-image.ts:270-272`.
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// Delete every transmitted image. `terminal-image.ts:278-280`.
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A,q=2\x1b\\".to_string()
}

/// Delete every placement, keeping the transmitted data. `terminal-image.ts:283-285`.
pub fn delete_all_kitty_placements() -> String {
    "\x1b_Ga=d,d=a,q=2\x1b\\".to_string()
}

/// iTerm2 inline image, BEL-terminated. `terminal-image.ts:287-312`.
pub fn encode_iterm2(base64_data: &str, options: &ITerm2EncodeOptions) -> String {
    let inline = i32::from(options.inline != Some(false));
    let mut params = vec![
        format!("inline={inline}"),
        format!("size={}", base64_decoded_len(base64_data)),
    ];
    if let Some(width) = &options.width {
        params.push(format!("width={width}"));
    }
    if let Some(height) = &options.height {
        params.push(format!("height={height}"));
    }
    if let Some(name) = options.name.as_deref().filter(|name| !name.is_empty()) {
        params.push(format!("name={}", base64_encode(name.as_bytes())));
    }
    if options.preserve_aspect_ratio == Some(false) {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("{ITERM2_PREFIX}{}:{base64_data}\x07", params.join(";"))
}

// ---------------------------------------------------------------------------
// Kitty metadata registry
// ---------------------------------------------------------------------------

/// Remember an image so a later placement/crop can size it. `terminal-image.ts:342-350`.
pub fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    KITTY_REGISTRY.with(|cell| {
        let mut registry = cell.borrow_mut();
        registry.generation += 1;
        let generation = registry.generation;
        registry
            .entries
            .retain(|entry| entry.metadata.image_id != metadata.image_id);
        registry.entries.push(RegisteredKittyImageMetadata {
            metadata,
            transmission_generation: generation,
        });
        if registry.entries.len() > MAX_KITTY_METADATA_ENTRIES {
            registry.entries.remove(0);
        }
    });
}

/// Forget every registered image. Test/host reset hook.
pub fn reset_kitty_image_metadata() {
    KITTY_REGISTRY.with(|cell| {
        let mut registry = cell.borrow_mut();
        registry.entries.clear();
        registry.generation = 0;
    });
}

/// Controls of the first Kitty command in `line` and the byte offset where it
/// starts. Mirrors `/\x1b_G([^;]*);/` from `terminal-image.ts:353`.
fn kitty_control_span(line: &str) -> Option<(usize, &str, usize)> {
    let start = line.find(KITTY_PREFIX)?;
    let body = &line[start + KITTY_PREFIX.len()..];
    let semicolon = body.find(';')?;
    let controls = &body[..semicolon];
    let control_len = controls.len();
    Some((
        start,
        controls,
        start + KITTY_PREFIX.len() + control_len + 1,
    ))
}

/// Registry lookup by `i=` control. `terminal-image.ts:352-357`.
pub fn get_registered_kitty_image_metadata(line: &str) -> Option<RegisteredKittyImageMetadata> {
    let (_, controls, _) = kitty_control_span(line)?;
    let image_id = controls
        .split(',')
        .find_map(|control| control.strip_prefix("i="))
        .and_then(|value| value.parse::<u32>().ok())?;
    KITTY_REGISTRY.with(|cell| {
        cell.borrow()
            .entries
            .iter()
            .find(|entry| entry.metadata.image_id == image_id)
            .copied()
    })
}

/// Metadata for a line, or `None` when the image was never registered.
/// `terminal-image.ts:359-369`.
pub fn get_kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    get_registered_kitty_image_metadata(line).map(|entry| entry.metadata)
}

/// Replace a Kitty transmission with a placement-only command.
/// `terminal-image.ts:392-424`.
///
/// Byte offsets are used where pi uses UTF-16 indices; the sequences this
/// parses are ASCII, so the two agree for every line the renderer emits.
pub fn get_kitty_image_placement(line: &str) -> Option<KittyImagePlacement> {
    let (match_start, first_controls, _) = kitty_control_span(line)?;
    let metadata = get_registered_kitty_image_metadata(line)?;

    let mut command_start = match_start;
    let mut command_controls = first_controls.to_string();
    let transmission_end;
    loop {
        let terminator = line[command_start + KITTY_PREFIX.len()..].find("\x1b\\")?
            + command_start
            + KITTY_PREFIX.len();
        let end = terminator + 2;
        let has_more = command_controls.split(',').any(|control| control == "m=1");
        if !has_more {
            transmission_end = end;
            break;
        }
        command_start = end;
        if !line[command_start..].starts_with(KITTY_PREFIX) {
            return None;
        }
        let body = &line[command_start + KITTY_PREFIX.len()..];
        let controls_end = body.find(';')?;
        command_controls = body[..controls_end].to_string();
    }

    let controls: Vec<&str> = first_controls
        .split(',')
        .filter(|control| {
            let key = control.split('=').next().unwrap_or("");
            KITTY_PLACEMENT_CONTROL_KEYS.contains(&key)
        })
        .collect();
    let sequence = format!("\x1b_Ga=p,q=2,{}\x1b\\", controls.join(","));
    let replacement_line = format!(
        "{}{}{}",
        &line[..match_start],
        sequence,
        &line[transmission_end..]
    );
    Some(KittyImagePlacement {
        image_id: metadata.metadata.image_id,
        transmission_generation: metadata.transmission_generation,
        transmission_bytes: transmission_end - match_start,
        estimated_decoded_bytes: u64::from(metadata.metadata.width_px)
            * u64::from(metadata.metadata.height_px)
            * ESTIMATED_DECODED_BYTES_PER_PIXEL,
        sequence,
        replacement_line,
    })
}

/// Rewrite a Kitty line to show a horizontal band of the image.
/// `terminal-image.ts:426-438`.
pub fn crop_kitty_image_line(line: &str, hidden_rows: usize, visible_rows: usize) -> String {
    let Some(metadata) = get_kitty_image_metadata(line) else {
        return line.to_string();
    };
    let Some((start, controls, after_controls)) = kitty_control_span(line) else {
        return line.to_string();
    };
    if hidden_rows >= metadata.rows || visible_rows == 0 {
        return line.to_string();
    }
    let cropped_rows = visible_rows.min(metadata.rows - hidden_rows);
    if hidden_rows == 0 && cropped_rows == metadata.rows {
        return line.to_string();
    }
    let height = f64::from(metadata.height_px);
    let rows = metadata.rows as f64;
    let source_y = ((height * hidden_rows as f64) / rows).floor();
    let source_end = ((height * (hidden_rows + cropped_rows) as f64) / rows).ceil();
    let source_height = (height.min(source_end) - source_y).max(1.0) as i64;

    let mut kept: Vec<String> = controls
        .split(',')
        .filter(|control| !matches!(control.split('=').next(), Some("y" | "h" | "r")))
        .map(str::to_string)
        .collect();
    kept.push(format!("y={source_y}"));
    kept.push(format!("h={source_height}"));
    kept.push(format!("r={cropped_rows}"));
    format!(
        "{}{KITTY_PREFIX}{};{}",
        &line[..start],
        kept.join(","),
        &line[after_controls..]
    )
}

// ---------------------------------------------------------------------------
// Cell sizing
// ---------------------------------------------------------------------------

/// Pick `upper` or `upper - 1`, whichever keeps the aspect distortion lower.
/// `terminal-image.ts:440-447`.
pub fn choose_less_distorted_cell_count(upper: usize, ideal: f64) -> usize {
    if upper <= 1 {
        return upper;
    }
    let lower = upper - 1;
    let upper_distortion = (upper as f64 / ideal).max(ideal / upper as f64);
    let lower_distortion = (lower as f64 / ideal).max(ideal / lower as f64);
    if lower_distortion < upper_distortion {
        lower
    } else {
        upper
    }
}

/// Cell footprint for an image. `terminal-image.ts:449-487`.
///
/// `optimize_aspect_ratio` is Kitty-only: iTerm2 keeps plain ceilings.
pub fn calculate_image_cell_size(
    image: &ImageDimensions,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
    cell: CellDimensions,
    optimize_aspect_ratio: bool,
) -> ImageCellSize {
    let max_width = max_width_cells.max(1);
    let max_height = max_height_cells.map(|height| height.max(1));
    let image_width = image.width_px.max(1) as f64;
    let image_height = image.height_px.max(1) as f64;
    let cell_width = cell.width_px as f64;
    let cell_height = cell.height_px as f64;

    let width_scale = (max_width as f64 * cell_width) / image_width;
    let height_scale = match max_height {
        Some(max_height) => (max_height as f64 * cell_height) / image_height,
        None => width_scale,
    };
    let scale = width_scale.min(height_scale);

    let scaled_width = image_width * scale;
    let scaled_height = image_height * scale;
    let mut columns = ((scaled_width / cell_width).ceil() as usize).clamp(1, max_width);
    let mut rows = ((scaled_height / cell_height).ceil() as usize).max(1);
    if let Some(max_height) = max_height {
        rows = rows.min(max_height);
    }

    if !optimize_aspect_ratio {
        return ImageCellSize { columns, rows };
    }

    if width_scale <= height_scale {
        let ideal_rows = (columns as f64 * cell_width * image_height) / (image_width * cell_height);
        rows = choose_less_distorted_cell_count(rows, ideal_rows);
    } else {
        let ideal_columns = (rows as f64 * cell_height * image_width) / (image_height * cell_width);
        columns = choose_less_distorted_cell_count(columns, ideal_columns);
    }
    ImageCellSize { columns, rows }
}

/// Row count for an image at a target width. `terminal-image.ts:489-495`.
pub fn calculate_image_rows(
    image: &ImageDimensions,
    target_width_cells: usize,
    cell: CellDimensions,
) -> usize {
    calculate_image_cell_size(image, target_width_cells, None, cell, false).rows
}

// ---------------------------------------------------------------------------
// Dimension probes
// ---------------------------------------------------------------------------

/// PNG IHDR size. `terminal-image.ts:497-516`.
pub fn get_png_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = base64_decode(base64_data);
    if buffer.len() < 24 || buffer[0..4] != [0x89, 0x50, 0x4e, 0x47] {
        return None;
    }
    Some(ImageDimensions {
        width_px: u32::from_be_bytes([buffer[16], buffer[17], buffer[18], buffer[19]]),
        height_px: u32::from_be_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]),
    })
}

/// JPEG SOF0/SOF1/SOF2 size. `terminal-image.ts:518-559`.
pub fn get_jpeg_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = base64_decode(base64_data);
    if buffer.len() < 2 || buffer[0] != 0xff || buffer[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < buffer.len() {
        if buffer[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = buffer[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([buffer[offset + 5], buffer[offset + 6]]);
            let width = u16::from_be_bytes([buffer[offset + 7], buffer[offset + 8]]);
            return Some(ImageDimensions {
                width_px: u32::from(width),
                height_px: u32::from(height),
            });
        }
        if offset + 3 >= buffer.len() {
            return None;
        }
        let length = u16::from_be_bytes([buffer[offset + 2], buffer[offset + 3]]);
        if length < 2 {
            return None;
        }
        offset += 2 + length as usize;
    }
    None
}

/// GIF logical screen size. `terminal-image.ts:561-581`.
pub fn get_gif_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = base64_decode(base64_data);
    if buffer.len() < 10 {
        return None;
    }
    let signature = &buffer[0..6];
    if signature != b"GIF87a" && signature != b"GIF89a" {
        return None;
    }
    Some(ImageDimensions {
        width_px: u32::from(u16::from_le_bytes([buffer[6], buffer[7]])),
        height_px: u32::from(u16::from_le_bytes([buffer[8], buffer[9]])),
    })
}

/// WebP VP8/VP8L/VP8X size. `terminal-image.ts:583-620`.
pub fn get_webp_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = base64_decode(base64_data);
    if buffer.len() < 30 || &buffer[0..4] != b"RIFF" || &buffer[8..12] != b"WEBP" {
        return None;
    }
    match &buffer[12..16] {
        b"VP8 " => Some(ImageDimensions {
            width_px: u32::from(u16::from_le_bytes([buffer[26], buffer[27]]) & 0x3fff),
            height_px: u32::from(u16::from_le_bytes([buffer[28], buffer[29]]) & 0x3fff),
        }),
        b"VP8L" => {
            let bits = u32::from_le_bytes([buffer[21], buffer[22], buffer[23], buffer[24]]);
            Some(ImageDimensions {
                width_px: (bits & 0x3fff) + 1,
                height_px: ((bits >> 14) & 0x3fff) + 1,
            })
        }
        b"VP8X" => Some(ImageDimensions {
            width_px: (u32::from(buffer[24])
                | (u32::from(buffer[25]) << 8)
                | (u32::from(buffer[26]) << 16))
                + 1,
            height_px: (u32::from(buffer[27])
                | (u32::from(buffer[28]) << 8)
                | (u32::from(buffer[29]) << 16))
                + 1,
        }),
        _ => None,
    }
}

/// Mime-dispatched size probe. `terminal-image.ts:622-636`.
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Inputs [`render_image_with`] needs instead of reading module state.
#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    pub capabilities: &'a TerminalCapabilities,
    pub cell_dimensions: CellDimensions,
}

/// Build the transmission for an image, or `None` when the terminal has no
/// inline image protocol. `terminal-image.ts:638-680`.
pub fn render_image(
    base64_data: &str,
    image: &ImageDimensions,
    options: &ImageRenderOptions,
) -> Option<RenderImageResult> {
    let capabilities = get_capabilities();
    let context = RenderContext {
        capabilities: &capabilities,
        cell_dimensions: get_cell_dimensions(),
    };
    render_image_with(base64_data, image, options, &context)
}

/// [`render_image`] against explicit capabilities and cell size, so tests
/// never touch the module cache.
pub fn render_image_with(
    base64_data: &str,
    image: &ImageDimensions,
    options: &ImageRenderOptions,
    context: &RenderContext<'_>,
) -> Option<RenderImageResult> {
    let capabilities = context.capabilities;
    if capabilities.images == ImageProtocol::Disabled {
        return None;
    }
    let max_width = options
        .max_width_cells
        .unwrap_or(RENDER_DEFAULT_MAX_WIDTH_CELLS);
    let size = calculate_image_cell_size(
        image,
        max_width,
        options.max_height_cells,
        context.cell_dimensions,
        capabilities.images == ImageProtocol::Kitty,
    );

    match capabilities.images {
        ImageProtocol::Kitty => {
            if let Some(image_id) = options.image_id {
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns: size.columns,
                    rows: size.rows,
                    width_px: image.width_px,
                    height_px: image.height_px,
                });
            }
            let sequence = encode_kitty(
                base64_data,
                KittyEncodeOptions {
                    columns: Some(size.columns),
                    rows: Some(size.rows),
                    image_id: options.image_id,
                    move_cursor: options.move_cursor,
                },
            );
            Some(RenderImageResult {
                sequence,
                columns: size.columns,
                rows: size.rows,
                image_id: options.image_id,
            })
        }
        ImageProtocol::Iterm2 => Some(RenderImageResult {
            sequence: encode_iterm2(
                base64_data,
                &ITerm2EncodeOptions {
                    width: Some(size.columns.to_string()),
                    height: Some("auto".to_string()),
                    preserve_aspect_ratio: Some(options.preserve_aspect_ratio.unwrap_or(true)),
                    ..ITerm2EncodeOptions::default()
                },
            ),
            columns: size.columns,
            rows: size.rows,
            image_id: None,
        }),
        ImageProtocol::Disabled => None,
    }
}

/// Wrap `text` in an OSC 8 hyperlink. `terminal-image.ts:700-702`.
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Shorten a home-prefixed path to `~/...`. `terminal-image.ts:705-712`.
pub fn shorten_image_path(filename: &str, home: Option<&str>) -> String {
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        // Exact home, or the POSIX/Windows separators pi checks.
        if filename == home
            || filename.starts_with(&format!("{home}/"))
            || filename.starts_with(&format!("{home}\\"))
        {
            return format!("~{}", &filename[home.len()..]);
        }
    }
    filename.to_string()
}

/// True for `/`-rooted and drive-rooted paths, like Node's `path.isAbsolute`.
fn is_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        || (path.len() > 2
            && path.as_bytes()[1] == b':'
            && matches!(path.as_bytes()[2], b'/' | b'\\'))
}

/// Plain-text placeholder for terminals without inline images.
/// `terminal-image.ts:718-731`.
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<&ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let home = CAPABILITY_ENV
        .with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|env| env.get("HOME").map(str::to_string))
        })
        .or_else(|| std::env::var("HOME").ok());
    image_fallback_with(
        mime_type,
        dimensions,
        filename,
        &get_capabilities(),
        home.as_deref(),
    )
}

/// [`image_fallback`] with explicit capabilities and home directory, so tests
/// never read `$HOME` or the capability cache.
pub fn image_fallback_with(
    mime_type: &str,
    dimensions: Option<&ImageDimensions>,
    filename: Option<&str>,
    capabilities: &TerminalCapabilities,
    home: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(filename) = filename.filter(|name| !name.is_empty()) {
        let display = shorten_image_path(filename, home);
        if capabilities.hyperlinks && is_absolute_path(filename) {
            let url = format!("file://{}", filename.replace('\\', "/"));
            parts.push(hyperlink(&display, &url));
        } else {
            parts.push(display);
        }
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(dimensions) = dimensions {
        parts.push(format!("{}x{}", dimensions.width_px, dimensions.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

// ---------------------------------------------------------------------------
// Inline base64
// ---------------------------------------------------------------------------

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding. Inline to keep pi-tui dependency-free.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(BASE64_ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn base64_value(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode standard base64, ignoring whitespace and stopping at `=`.
pub fn base64_decode(data: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in data.bytes() {
        if byte == b'=' {
            break;
        }
        let Some(value) = base64_value(byte) else {
            continue;
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    out
}

/// Byte length of the decoded payload, computed arithmetically.
/// `terminal-image.ts:291`.
fn base64_decoded_len(data: &str) -> usize {
    base64_decode(data).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> TerminalEnv {
        let mut env = TerminalEnv::new();
        for (key, value) in pairs {
            env.set(key, value);
        }
        env
    }

    /// Probe stand-in; counts calls so "probe never runs" stays assertable.
    /// No `tmux` process is ever spawned in tests.
    type CallCount = Rc<RefCell<usize>>;

    fn counting_probe(
        result: Option<&'static str>,
    ) -> (Box<dyn Fn() -> Option<String>>, CallCount) {
        let calls = Rc::new(RefCell::new(0));
        let counter = calls.clone();
        let probe: Box<dyn Fn() -> Option<String>> = Box::new(move || {
            *counter.borrow_mut() += 1;
            result.map(str::to_string)
        });
        (probe, calls)
    }

    fn kitty_caps() -> TerminalCapabilities {
        TerminalCapabilities {
            images: ImageProtocol::Kitty,
            true_color: true,
            hyperlinks: true,
        }
    }

    fn iterm_caps() -> TerminalCapabilities {
        TerminalCapabilities {
            images: ImageProtocol::Iterm2,
            true_color: true,
            hyperlinks: true,
        }
    }

    fn no_caps() -> TerminalCapabilities {
        TerminalCapabilities::default()
    }

    fn strip_hyperlinks(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("\x1b]8;;") {
            let after = &rest[start + 5..];
            match after.find("\x1b\\") {
                Some(end) => {
                    out.push_str(&rest[..start]);
                    rest = &after[end + 2..];
                }
                None => break,
            }
        }
        out.push_str(rest);
        out
    }

    // --- AC 1-22: isImageLine ------------------------------------------------

    #[test]
    fn ac01_iterm2_at_line_start() {
        assert!(is_image_line(
            "\x1b]1337;File=size=100,100;inline=1:base64encodeddata==\x07"
        ));
    }

    #[test]
    fn ac02_iterm2_with_text_around() {
        assert!(is_image_line(
            "Some text \x1b]1337;File=size=100,100;inline=1:base64data==\x07 more text"
        ));
    }

    #[test]
    fn ac03_iterm2_middle_of_long_line() {
        assert!(is_image_line(&format!(
            "{}ITerm{}{}",
            "Text before image...".repeat(3),
            "\x1b]1337;File=inline=1:verylongbase64data==",
            "...text after"
        )));
    }

    #[test]
    fn ac04_iterm2_at_end_of_line() {
        assert!(is_image_line(
            "Regular text ending with \x1b]1337;File=inline=1:base64data==\x07"
        ));
    }

    #[test]
    fn ac05_minimal_iterm2() {
        assert!(is_image_line("\x1b]1337;File=:\x07"));
    }

    #[test]
    fn ac06_kitty_at_line_start() {
        assert!(is_image_line(
            "\x1b_Ga=T,f=100,t=f,d=base64data...\x1b\\\x1b_Gm=i=1;\x1b\\"
        ));
    }

    #[test]
    fn ac07_kitty_with_text_before() {
        assert!(is_image_line(
            "Output: \x1b_Ga=T,f=100;data...\x1b\\\x1b_Gm=i=1;\x1b\\"
        ));
    }

    #[test]
    fn ac08_kitty_with_padding() {
        assert!(is_image_line(
            "  \x1b_Ga=T,f=100...\x1b\\\x1b_Gm=i=1;\x1b\\  "
        ));
    }

    #[test]
    fn ac09_long_line_with_image() {
        let line = format!(
            "\x1b]1337;File=size=800,600;inline=1:{}",
            "A".repeat(100).repeat(3000)
        );
        assert!(line.len() > 300_000);
        assert!(is_image_line(&line));
    }

    #[test]
    fn ac10_image_on_non_image_terminal() {
        assert!(is_image_line(
            "Read image file [image/jpeg]\x1b]1337;File=inline=1:base64data==\x07"
        ));
    }

    #[test]
    fn ac11_ansi_before_image() {
        assert!(is_image_line(
            "\x1b[31mError output \x1b]1337;File=inline=1:image==\x07"
        ));
    }

    #[test]
    fn ac12_ansi_after_image() {
        assert!(is_image_line(
            "\x1b_Ga=T,f=100:data...\x1b\\\x1b_Gm=i=1;\x1b\\\x1b[0m reset"
        ));
    }

    #[test]
    fn ac13_plain_text_is_not_image() {
        assert!(!is_image_line("Just some plain text"));
    }

    #[test]
    fn ac14_ansi_only_text_is_not_image() {
        assert!(!is_image_line(
            "\x1b[31mRed text\x1b[0m and \x1b[32mgreen text\x1b[0m"
        ));
    }

    #[test]
    fn ac15_cursor_movement_is_not_image() {
        assert!(!is_image_line("\x1b[1A\x1b[2KLine cleared and moved up"));
    }

    #[test]
    fn ac16_partial_iterm2_prefix_without_esc() {
        assert!(!is_image_line(
            "Some text with ]1337;File but missing ESC at start"
        ));
    }

    #[test]
    fn ac17_partial_kitty_prefix_without_esc() {
        assert!(!is_image_line("Some text with _G but missing ESC at start"));
    }

    #[test]
    fn ac18_empty_line() {
        assert!(!is_image_line(""));
    }

    #[test]
    fn ac19_newlines_only() {
        assert!(!is_image_line("\n"));
        assert!(!is_image_line("\n\n"));
    }

    #[test]
    fn ac20_both_protocols_in_one_line() {
        assert!(is_image_line(
            "\x1b_Ga=T;AAA\x1b\\ text \x1b]1337;File=:AAA\x07"
        ));
    }

    #[test]
    fn ac21_two_iterm2_segments() {
        assert!(is_image_line(
            "\x1b]1337;File=:AAA\x07 mid \x1b]1337;File=:BBB\x07"
        ));
    }

    #[test]
    fn ac22_path_containing_keywords_is_not_image() {
        assert!(!is_image_line("/path/to/File_1337_backup/image.jpg"));
    }

    // --- AC 23-48: detectCapabilities ---------------------------------------

    #[test]
    fn ac23_empty_env() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&TerminalEnv::new(), &probe);
        assert_eq!(caps.images, ImageProtocol::Disabled);
        assert!(!caps.hyperlinks);
        assert!(!caps.true_color);
    }

    #[test]
    fn ac24_all_overrides_forced_on() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(
            &env(&[
                ("PI_HYPERLINKS", "1"),
                ("PI_IMAGE_PROTOCOL", "kitty"),
                ("PI_TRUE_COLOR", "1"),
            ]),
            &probe,
        );
        assert_eq!(caps.images, ImageProtocol::Kitty);
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ac25_all_overrides_forced_off() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(
            &env(&[
                ("TERM_PROGRAM", "iterm.app"),
                ("PI_HYPERLINKS", "0"),
                ("PI_IMAGE_PROTOCOL", "none"),
                ("PI_TRUE_COLOR", "0"),
            ]),
            &probe,
        );
        assert_eq!(caps.images, ImageProtocol::Disabled);
        assert!(!caps.true_color);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn ac26_auto_overrides_are_ignored() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(
            &env(&[
                ("TERM_PROGRAM", "ghostty"),
                ("PI_IMAGE_PROTOCOL", "auto"),
                ("PI_TRUE_COLOR", "auto"),
                ("PI_HYPERLINKS", "auto"),
            ]),
            &probe,
        );
        assert_eq!(caps.images, ImageProtocol::Kitty);
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ac27_overrides_apply_then_clear() {
        // Exercises the module-level override layering of
        // `getCapabilities()` (`terminal-image.ts:161-192`) without touching
        // the real environment or spawning tmux.
        set_capability_environment(Some(env(&[("TERM_PROGRAM", "ghostty")])));
        set_tmux_probe(Some(Box::new(|| None)));
        set_capability_overrides(PartialTerminalCapabilities {
            images: Some(Some(ImageProtocol::Disabled)),
            true_color: Some(false),
            hyperlinks: Some(false),
        });
        let overridden = get_capabilities();
        assert_eq!(overridden.images, ImageProtocol::Disabled);
        assert!(!overridden.true_color);
        assert!(!overridden.hyperlinks);

        set_capability_overrides(PartialTerminalCapabilities::default());
        let restored = get_capabilities();
        assert_eq!(restored.images, ImageProtocol::Kitty);
        assert!(restored.true_color);
        assert!(restored.hyperlinks);

        set_tmux_probe(None);
        set_capability_environment(None);
        reset_capabilities_cache();
    }

    #[test]
    fn ac28_hyperlink_override_skips_probe_under_tmux() {
        let (probe, calls) = counting_probe(Some("hyperlinks"));
        let caps = detect_capabilities(
            &env(&[
                ("TMUX", "1"),
                ("PI_HYPERLINKS", "1"),
                ("PI_IMAGE_PROTOCOL", "kitty"),
            ]),
            &probe,
        );
        assert_eq!(*calls.borrow(), 0);
        assert!(caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Kitty);
    }

    #[test]
    fn ac29_tmux_env_with_ghostty_probe_true() {
        let (probe, _) = counting_probe(Some("hyperlinks"));
        let caps = detect_capabilities(&env(&[("TMUX", "1"), ("TERM_PROGRAM", "ghostty")]), &probe);
        assert!(caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    #[test]
    fn ac30_tmux_env_with_ghostty_probe_false() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&env(&[("TMUX", "1"), ("TERM_PROGRAM", "ghostty")]), &probe);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    #[test]
    fn ac31_tmux_term_with_iterm() {
        let (probe, _) = counting_probe(Some("hyperlinks"));
        let with_hyperlinks = detect_capabilities(
            &env(&[("TERM", "tmux-256color"), ("TERM_PROGRAM", "iterm.app")]),
            &probe,
        );
        assert!(with_hyperlinks.hyperlinks);
        assert_eq!(with_hyperlinks.images, ImageProtocol::Disabled);

        let (probe, _) = counting_probe(None);
        let without = detect_capabilities(
            &env(&[("TERM", "tmux-256color"), ("TERM_PROGRAM", "iterm.app")]),
            &probe,
        );
        assert!(!without.hyperlinks);
    }

    #[test]
    fn ac32_screen_disables_images_and_links() {
        let (probe, _) = counting_probe(Some("hyperlinks"));
        let caps = detect_capabilities(&env(&[("TERM", "screen-256color")]), &probe);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    #[test]
    fn ac33_ghostty_term_program() {
        let (probe, _) = counting_probe(None);
        assert!(detect_capabilities(&env(&[("TERM_PROGRAM", "ghostty")]), &probe).hyperlinks);
    }

    #[test]
    fn ac34_cmux_workspace_id_is_inert() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(
            &env(&[
                ("TERM_PROGRAM", "ghostty"),
                ("CMUX_WORKSPACE_ID", "workspace"),
            ]),
            &probe,
        );
        assert_eq!(caps.images, ImageProtocol::Kitty);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ac35_kitty_window_id() {
        let (probe, _) = counting_probe(None);
        assert!(detect_capabilities(&env(&[("KITTY_WINDOW_ID", "1")]), &probe).hyperlinks);
    }

    #[test]
    fn ac36_wezterm_pane_zero_is_set() {
        let (probe, _) = counting_probe(None);
        assert!(detect_capabilities(&env(&[("WEZTERM_PANE", "0")]), &probe).hyperlinks);
    }

    #[test]
    fn ac37_warp_term_program() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&env(&[("TERM_PROGRAM", "WarpTerminal")]), &probe);
        assert_eq!(caps.images, ImageProtocol::Kitty);
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ac38_warp_session_id() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&env(&[("WARP_SESSION_ID", "some-session-id")]), &probe);
        assert_eq!(caps.images, ImageProtocol::Kitty);
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ac39_warp_terminal_session_uuid() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(
            &env(&[(
                "WARP_TERMINAL_SESSION_UUID",
                "d0e1a2e5-7ca7-44cd-9037-ac7222011161",
            )]),
            &probe,
        );
        assert_eq!(caps.images, ImageProtocol::Kitty);
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ac40_warp_under_tmux() {
        let (probe, _) = counting_probe(Some("hyperlinks"));
        let caps = detect_capabilities(
            &env(&[
                ("TERM_PROGRAM", "warpterminal"),
                ("TMUX", "1"),
                ("TERM", "tmux-256color"),
            ]),
            &probe,
        );
        assert_eq!(caps.images, ImageProtocol::Disabled);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ac41_iterm_term_program() {
        let (probe, _) = counting_probe(None);
        assert!(detect_capabilities(&env(&[("TERM_PROGRAM", "iterm.app")]), &probe).hyperlinks);
    }

    #[test]
    fn ac42_vscode_term_program() {
        let (probe, _) = counting_probe(None);
        assert!(detect_capabilities(&env(&[("TERM_PROGRAM", "vscode")]), &probe).hyperlinks);
    }

    #[test]
    fn ac43_zed_term_program() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&env(&[("TERM_PROGRAM", "zed")]), &probe);
        assert_eq!(
            caps,
            TerminalCapabilities {
                images: ImageProtocol::Disabled,
                true_color: true,
                hyperlinks: true
            }
        );
    }

    #[test]
    fn ac44_wt_session() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(
            &env(&[("WT_SESSION", "session"), ("TERM", "xterm-256color")]),
            &probe,
        );
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    #[test]
    fn ac45_jetbrains_jediterm() {
        let (probe, _) = counting_probe(None);
        let caps =
            detect_capabilities(&env(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")]), &probe);
        assert!(caps.true_color);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    #[test]
    fn ac46_wt_session_under_tmux_loses_truecolor() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(
            &env(&[
                ("WT_SESSION", "s"),
                ("TMUX", "1"),
                ("TERM", "tmux-256color"),
            ]),
            &probe,
        );
        assert!(!caps.true_color);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    #[test]
    fn ac47_colorterm_hint_survives_tmux() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&env(&[("COLORTERM", "truecolor"), ("TMUX", "1")]), &probe);
        assert!(caps.true_color);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn ac48_direct_term_hint() {
        let (probe, _) = counting_probe(None);
        assert!(detect_capabilities(&env(&[("TERM", "xterm-direct")]), &probe).true_color);
    }

    #[test]
    fn windows_console_branch() {
        let (probe, _) = counting_probe(None);
        let mut environment = TerminalEnv::new();
        environment.is_windows = true;
        let caps = detect_capabilities(&environment, &probe);
        assert!(caps.true_color);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    #[test]
    fn cmux_kitty_branch_via_term() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&env(&[("TERM", "xterm-ghostty")]), &probe);
        assert_eq!(caps.images, ImageProtocol::Kitty);
    }

    #[test]
    fn empty_env_values_are_unset() {
        let (probe, _) = counting_probe(None);
        let caps = detect_capabilities(&env(&[("TMUX", ""), ("WT_SESSION", "")]), &probe);
        assert_eq!(caps.images, ImageProtocol::Disabled);
    }

    // --- AC 49: iTerm2 encoding --------------------------------------------

    #[test]
    fn ac49_iterm2_encode_literal() {
        let options = ITerm2EncodeOptions {
            width: Some("2".to_string()),
            height: Some("auto".to_string()),
            ..ITerm2EncodeOptions::default()
        };
        assert_eq!(
            encode_iterm2("AAAA", &options),
            "\x1b]1337;File=inline=1;size=3;width=2;height=auto:AAAA\x07"
        );
    }

    // --- AC 50-65: Kitty encoding / placement / cropping / sizing ----------

    #[test]
    fn ac50_kitty_encode_param_order() {
        let sequence = encode_kitty(
            "AAAA",
            KittyEncodeOptions {
                columns: Some(2),
                rows: Some(2),
                move_cursor: Some(false),
                image_id: None,
            },
        );
        assert!(
            sequence.starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=2,r=2;"),
            "{sequence}"
        );
        assert!(sequence.ends_with("\x1b\\"));
    }

    #[test]
    fn ac51_delete_sequences() {
        assert_eq!(delete_kitty_image(42), "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
        assert_eq!(delete_all_kitty_images(), "\x1b_Ga=d,d=A,q=2\x1b\\");
        assert_eq!(delete_all_kitty_placements(), "\x1b_Ga=d,d=a,q=2\x1b\\");
    }

    #[test]
    fn kitty_chunking_is_4096_wide_with_m0_last() {
        let data = "A".repeat(4096);
        let sequence = encode_kitty(&data, KittyEncodeOptions::default());
        // pi treats exactly CHUNK_SIZE as unchunked (`length <= CHUNK_SIZE`).
        assert_eq!(sequence, format!("\x1b_Ga=T,f=100,q=2;{data}\x1b\\"));

        let short = encode_kitty(&"A".repeat(4097), KittyEncodeOptions::default());
        assert!(
            short.starts_with("\x1b_Ga=T,f=100,q=2,m=1;"),
            "{}",
            &short[..40]
        );
        assert!(
            short.ends_with("\x1b_Gm=0;A\x1b\\"),
            "{}",
            &short[short.len() - 20..]
        );
        assert_eq!(
            short.matches("\x1b_G").count(),
            2,
            "first + last chunk only"
        );

        // Three chunks: middle chunk is a bare `m=1` command.
        let three = encode_kitty(&"A".repeat(9000), KittyEncodeOptions::default());
        assert_eq!(three.matches("\x1b_G").count(), 3);
        assert!(three.contains("\x1b_Gm=1;AAAA"), "{}", &three[4220..4280]);

        // Exact multiple of the chunk size: pi ends on a full `m=0` chunk.
        let exact = encode_kitty(&"A".repeat(8192), KittyEncodeOptions::default());
        assert_eq!(exact.matches("\x1b_G").count(), 2);
        assert!(exact.ends_with(&format!("\x1b_Gm=0;{}\x1b\\", "A".repeat(4096))));
    }

    #[test]
    fn ac52_render_kitty_no_cursor_suppression() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 10,
                height_px: 10,
            },
        };
        let result = render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 20,
                height_px: 20,
            },
            &ImageRenderOptions {
                max_width_cells: Some(2),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert!(!result.sequence.contains(",C=1,"));
        assert_eq!(result.rows, 2);
    }

    #[test]
    fn ac53_render_kitty_with_move_cursor_false() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 10,
                height_px: 10,
            },
        };
        let result = render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 20,
                height_px: 20,
            },
            &ImageRenderOptions {
                max_width_cells: Some(2),
                move_cursor: Some(false),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert!(result.sequence.contains(",C=1,"));
        assert_eq!(result.rows, 2);
    }

    #[test]
    fn ac54_crop_of_registered_kitty_line() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 10,
                height_px: 10,
            },
        };
        let result = render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 100,
                height_px: 100,
            },
            &ImageRenderOptions {
                max_width_cells: Some(3),
                image_id: Some(42),
                move_cursor: Some(false),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(
            get_kitty_image_metadata(&result.sequence),
            Some(KittyImageMetadata {
                image_id: 42,
                columns: 3,
                rows: 3,
                width_px: 100,
                height_px: 100
            })
        );
        let cropped = crop_kitty_image_line(&result.sequence, 2, 1);
        assert!(cropped.contains("y=66,h=34,r=1"), "{cropped}");
    }

    #[test]
    fn ac55_placement_of_cropped_chunked_line() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 10,
                height_px: 10,
            },
        };
        render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 100,
                height_px: 100,
            },
            &ImageRenderOptions {
                max_width_cells: Some(3),
                image_id: Some(42),
                move_cursor: Some(false),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        register_kitty_image_metadata(KittyImageMetadata {
            image_id: 42,
            columns: 3,
            rows: 3,
            width_px: 100,
            height_px: 100,
        });
        let sequence = encode_kitty(
            &"A".repeat(8192),
            KittyEncodeOptions {
                columns: Some(3),
                rows: Some(3),
                image_id: Some(42),
                move_cursor: Some(false),
            },
        );
        let line = format!("left {} right", crop_kitty_image_line(&sequence, 2, 1));
        let placement = get_kitty_image_placement(&line).expect("placement");
        assert_eq!(
            placement.transmission_bytes,
            line.len() - "left ".len() - " right".len()
        );
        assert_eq!(placement.estimated_decoded_bytes, 100 * 100 * 4);
        assert_eq!(
            placement.sequence,
            "\x1b_Ga=p,q=2,C=1,c=3,i=42,y=66,h=34,r=1\x1b\\"
        );
        assert_eq!(
            placement.replacement_line,
            format!("left {} right", placement.sequence)
        );
        assert!(!placement.replacement_line.contains("AAAA"));
    }

    #[test]
    fn ac56_render_with_max_height_cells() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 10,
                height_px: 10,
            },
        };
        let result = render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 10,
                height_px: 100,
            },
            &ImageRenderOptions {
                max_width_cells: Some(10),
                max_height_cells: Some(5),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(result.rows, 5);
        assert!(result.sequence.contains(",c=1,r=5"), "{}", result.sequence);
    }

    #[test]
    fn ac60_wide_short_image_is_one_row() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 9,
                height_px: 18,
            },
        };
        let result = render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 1200,
                height_px: 12,
            },
            &ImageRenderOptions {
                max_width_cells: Some(60),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(result.rows, 1);
        assert!(
            result.sequence.contains(",c=60,r=1;"),
            "{}",
            result.sequence
        );
    }

    #[test]
    fn ac61_issue_8938_layout_and_placement() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 9,
                height_px: 18,
            },
        };
        let image = ImageDimensions {
            width_px: 615,
            height_px: 86,
        };
        let first = render_image_with(
            "AAAA",
            &image,
            &ImageRenderOptions {
                max_width_cells: Some(60),
                max_height_cells: Some(30),
                image_id: Some(8938),
                move_cursor: Some(false),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(first.rows, 4);
        assert!(
            first.sequence.contains(",c=60,r=4,i=8938;"),
            "{}",
            first.sequence
        );
        assert_eq!(
            get_kitty_image_metadata(&first.sequence),
            Some(KittyImageMetadata {
                image_id: 8938,
                columns: 60,
                rows: 4,
                width_px: 615,
                height_px: 86
            })
        );
        let placement = get_kitty_image_placement(&crop_kitty_image_line(&first.sequence, 1, 2))
            .expect("placement");
        assert_eq!(
            placement.sequence,
            "\x1b_Ga=p,q=2,C=1,c=60,i=8938,y=21,h=44,r=2\x1b\\"
        );

        let second = render_image_with(
            "AAAA",
            &image,
            &ImageRenderOptions {
                max_width_cells: Some(30),
                max_height_cells: Some(15),
                image_id: Some(8938),
                move_cursor: Some(false),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(second.rows, 2);
        assert!(
            second.sequence.contains(",c=30,r=2,i=8938;"),
            "{}",
            second.sequence
        );
        assert_eq!(
            get_kitty_image_metadata(&second.sequence).map(|m| m.rows),
            Some(2)
        );
    }

    #[test]
    fn ac62_larger_cells_give_five_rows() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 15,
                height_px: 28,
            },
        };
        let result = render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 615,
                height_px: 86,
            },
            &ImageRenderOptions {
                max_width_cells: Some(60),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(result.rows, 5);
        assert!(
            result.sequence.contains(",c=60,r=5;"),
            "{}",
            result.sequence
        );
    }

    #[test]
    fn ac63_tall_image_narrow_columns() {
        reset_kitty_image_metadata();
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 14,
                height_px: 28,
            },
        };
        let image = ImageDimensions {
            width_px: 400,
            height_px: 900,
        };
        let first = render_image_with(
            "AAAA",
            &image,
            &ImageRenderOptions {
                max_width_cells: Some(30),
                max_height_cells: Some(15),
                image_id: Some(8938),
                move_cursor: Some(false),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(first.rows, 15);
        assert!(
            first.sequence.contains(",c=13,r=15,i=8938;"),
            "{}",
            first.sequence
        );
        assert_eq!(
            get_kitty_image_metadata(&first.sequence),
            Some(KittyImageMetadata {
                image_id: 8938,
                columns: 13,
                rows: 15,
                width_px: 400,
                height_px: 900
            })
        );
        let placement = get_kitty_image_placement(&crop_kitty_image_line(&first.sequence, 1, 2))
            .expect("placement");
        assert_eq!(
            placement.sequence,
            "\x1b_Ga=p,q=2,C=1,c=13,i=8938,y=60,h=120,r=2\x1b\\"
        );

        let second = render_image_with(
            "AAAA",
            &image,
            &ImageRenderOptions {
                max_width_cells: Some(20),
                max_height_cells: Some(10),
                image_id: Some(8938),
                move_cursor: Some(false),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("kitty renders");
        assert_eq!(second.rows, 10);
        assert!(
            second.sequence.contains(",c=9,r=10,i=8938;"),
            "{}",
            second.sequence
        );
    }

    #[test]
    fn ac64_unit_cells_saturate_the_row_budget() {
        let context = RenderContext {
            capabilities: &kitty_caps(),
            cell_dimensions: CellDimensions {
                width_px: 1,
                height_px: 1,
            },
        };
        for (width_px, columns) in [(1u32, 1usize), (140, 1), (149, 2)] {
            reset_kitty_image_metadata();
            let result = render_image_with(
                "AAAA",
                &ImageDimensions {
                    width_px,
                    height_px: 1000,
                },
                &ImageRenderOptions {
                    max_width_cells: Some(30),
                    max_height_cells: Some(10),
                    ..ImageRenderOptions::default()
                },
                &context,
            )
            .expect("kitty renders");
            assert_eq!(result.rows, 10, "width {width_px}");
            assert!(
                result.sequence.contains(&format!("c={columns},r=10;")),
                "{}",
                result.sequence
            );
        }
    }

    #[test]
    fn ac65_iterm2_uses_plain_ceilings() {
        let context = RenderContext {
            capabilities: &iterm_caps(),
            cell_dimensions: CellDimensions {
                width_px: 14,
                height_px: 28,
            },
        };
        let result = render_image_with(
            "AAAA",
            &ImageDimensions {
                width_px: 400,
                height_px: 900,
            },
            &ImageRenderOptions {
                max_width_cells: Some(30),
                max_height_cells: Some(15),
                ..ImageRenderOptions::default()
            },
            &context,
        )
        .expect("iterm2 renders");
        assert_eq!(result.columns, 14);
        assert_eq!(result.rows, 15);
        assert_eq!(
            result.sequence,
            "\x1b]1337;File=inline=1;size=3;width=14;height=auto:AAAA\x07"
        );
    }

    #[test]
    fn disabled_terminal_renders_nothing() {
        let context = RenderContext {
            capabilities: &no_caps(),
            cell_dimensions: CellDimensions::default(),
        };
        assert!(
            render_image_with(
                "AAAA",
                &ImageDimensions {
                    width_px: 1,
                    height_px: 1
                },
                &ImageRenderOptions::default(),
                &context,
            )
            .is_none()
        );
    }

    #[test]
    fn cell_size_helpers() {
        let image = ImageDimensions {
            width_px: 400,
            height_px: 900,
        };
        let size = calculate_image_cell_size(
            &image,
            30,
            Some(10),
            CellDimensions {
                width_px: 9,
                height_px: 18,
            },
            false,
        );
        assert!(size.columns >= 1 && size.columns <= 30);
        assert!(size.rows <= 10);
        let unbounded =
            calculate_image_cell_size(&image, 30, None, CellDimensions::default(), false);
        assert_eq!(
            calculate_image_rows(&image, 30, CellDimensions::default()),
            unbounded.rows
        );
        assert_eq!(choose_less_distorted_cell_count(1, 0.5), 1);
        assert_eq!(choose_less_distorted_cell_count(3, 3.0), 3);
        assert_eq!(choose_less_distorted_cell_count(3, 2.097), 2);
        assert_eq!(choose_less_distorted_cell_count(3, 2.9), 3);
    }

    // --- AC 67-70: imageFallback -------------------------------------------

    #[test]
    fn ac67_fallback_without_hyperlinks() {
        assert_eq!(
            image_fallback_with(
                "image/png",
                Some(&ImageDimensions {
                    width_px: 1280,
                    height_px: 720
                }),
                Some("/home/user/.pi/agent/shot.png"),
                &no_caps(),
                Some("/home/user"),
            ),
            "[Image: ~/.pi/agent/shot.png [image/png] 1280x720]"
        );
    }

    #[test]
    fn ac68_fallback_with_hyperlinks() {
        let capabilities = TerminalCapabilities {
            images: ImageProtocol::Disabled,
            true_color: false,
            hyperlinks: true,
        };
        let rendered = image_fallback_with(
            "image/png",
            Some(&ImageDimensions {
                width_px: 10,
                height_px: 10,
            }),
            Some("/home/user/.pi/agent/shot.png"),
            &capabilities,
            Some("/home/user"),
        );
        assert!(rendered.contains("\x1b]8;;file://"), "{rendered}");
        assert!(
            rendered.contains("/home/user/.pi/agent/shot.png"),
            "{rendered}"
        );
        assert_eq!(
            strip_hyperlinks(&rendered),
            "[Image: ~/.pi/agent/shot.png [image/png] 10x10]"
        );
    }

    #[test]
    fn ac69_relative_filename_is_not_linked() {
        let capabilities = TerminalCapabilities {
            images: ImageProtocol::Disabled,
            true_color: false,
            hyperlinks: true,
        };
        let rendered = image_fallback_with(
            "image/png",
            Some(&ImageDimensions {
                width_px: 1,
                height_px: 1,
            }),
            Some("clankolas.png"),
            &capabilities,
            None,
        );
        assert_eq!(rendered, "[Image: clankolas.png [image/png] 1x1]");
        assert!(!rendered.contains("\x1b]8;"));
    }

    #[test]
    fn ac70_fallback_without_filename() {
        assert_eq!(
            image_fallback_with(
                "image/png",
                Some(&ImageDimensions {
                    width_px: 8,
                    height_px: 6
                }),
                None,
                &no_caps(),
                None,
            ),
            "[Image: [image/png] 8x6]"
        );
    }

    // --- AC 71-74: hyperlink ------------------------------------------------

    #[test]
    fn ac71_hyperlink_literal() {
        assert_eq!(
            hyperlink("click me", "https://example.com"),
            "\x1b]8;;https://example.com\x1b\\click me\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn ac72_hyperlink_preserves_styling() {
        let styled = "\x1b[4m\x1b[34mclick me\x1b[0m";
        let rendered = hyperlink(styled, "https://example.com");
        assert!(rendered.starts_with("\x1b]8;;https://example.com\x1b\\"));
        assert!(rendered.contains(styled));
        assert!(rendered.ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn ac73_hyperlink_empty_text() {
        assert_eq!(
            hyperlink("", "https://example.com"),
            "\x1b]8;;https://example.com\x1b\\\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn ac74_hyperlink_file_url() {
        let rendered = hyperlink("README.md", "file:///home/user/README.md");
        assert!(rendered.contains("file:///home/user/README.md"));
        assert!(rendered.contains("README.md"));
    }

    // --- Base64, ids, cell dimensions, registry ----------------------------

    #[test]
    fn base64_round_trip() {
        assert_eq!(base64_encode(b"AAA"), "QUFB");
        assert_eq!(base64_encode(b"AA"), "QUE=");
        assert_eq!(base64_encode(b"A"), "QQ==");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_decode("QUFB"), b"AAA".to_vec());
        assert_eq!(base64_decode("QQ=="), b"A".to_vec());
        assert_eq!(base64_encode(b"a\xffb"), base64_encode(&[0x61, 0xff, 0x62]));
        assert_eq!(base64_decode(&base64_encode(b"a\xffb")), b"a\xffb".to_vec());
        assert_eq!(base64_decoded_len("AAAA"), 3);
    }

    #[test]
    fn iterm2_name_is_base64_encoded() {
        let options = ITerm2EncodeOptions {
            name: Some("shot.png".to_string()),
            inline: Some(false),
            preserve_aspect_ratio: Some(false),
            ..ITerm2EncodeOptions::default()
        };
        assert_eq!(
            encode_iterm2("AAAA", &options),
            format!(
                "\x1b]1337;File=inline=0;size=3;name={};preserveAspectRatio=0:AAAA\x07",
                base64_encode(b"shot.png")
            )
        );
    }

    #[test]
    fn image_ids_stay_in_range() {
        for seed in [0u64, 1, 42, u64::MAX, 0xffff_fffe] {
            let id = allocate_image_id_from_seed(seed);
            assert!((1..=0xffff_fffe).contains(&id), "seed {seed} -> {id}");
        }
        assert_ne!(allocate_image_id(), 0);
    }

    #[test]
    fn cell_dimensions_round_trip() {
        assert_eq!(
            get_cell_dimensions(),
            CellDimensions {
                width_px: 9,
                height_px: 18
            }
        );
        set_cell_dimensions(CellDimensions {
            width_px: 12,
            height_px: 24,
        });
        assert_eq!(
            get_cell_dimensions(),
            CellDimensions {
                width_px: 12,
                height_px: 24
            }
        );
        set_cell_dimensions(CellDimensions::default());
    }

    #[test]
    fn color_mode_follows_true_color() {
        assert_eq!(
            get_terminal_color_mode(&TerminalCapabilities {
                true_color: true,
                ..no_caps()
            }),
            TerminalColorMode::TrueColor
        );
        assert_eq!(
            get_terminal_color_mode(&no_caps()),
            TerminalColorMode::Color256
        );
    }

    #[test]
    fn registry_evicts_the_oldest_entry() {
        reset_kitty_image_metadata();
        for id in 0..=(MAX_KITTY_METADATA_ENTRIES as u32) {
            register_kitty_image_metadata(KittyImageMetadata {
                image_id: id,
                columns: 1,
                rows: 1,
                width_px: 1,
                height_px: 1,
            });
        }
        assert!(get_kitty_image_metadata("\x1b_Ga=T,i=0,x=0;\x1b\\").is_none());
        let last = MAX_KITTY_METADATA_ENTRIES as u32;
        assert!(get_kitty_image_metadata(&format!("\x1b_Ga=T,i={last};\x1b\\")).is_some());
        reset_kitty_image_metadata();
    }

    #[test]
    fn crop_guards_leave_the_line_alone() {
        reset_kitty_image_metadata();
        register_kitty_image_metadata(KittyImageMetadata {
            image_id: 7,
            columns: 4,
            rows: 4,
            width_px: 100,
            height_px: 100,
        });
        let line = "\x1b_Ga=T,f=100,q=2,C=1,c=4,r=4,i=7;AAAA\x1b\\";
        assert_eq!(crop_kitty_image_line(line, 4, 2), line);
        assert_eq!(crop_kitty_image_line(line, 0, 0), line);
        assert_eq!(crop_kitty_image_line(line, 0, 4), line);
        assert_eq!(crop_kitty_image_line("no image", 1, 1), "no image");
    }

    #[test]
    fn dimension_probes() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0, 0, 4, 0]); // width 1024
        png.extend_from_slice(&[0, 0, 3, 0]); // height 768
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        let png = base64_encode(&png);
        assert_eq!(
            get_png_dimensions(&png),
            Some(ImageDimensions {
                width_px: 1024,
                height_px: 768
            })
        );
        assert_eq!(
            get_image_dimensions(&png, "image/png"),
            get_png_dimensions(&png)
        );
        assert_eq!(get_image_dimensions(&png, "image/tiff"), None);
        assert_eq!(get_png_dimensions("AAAA"), None);

        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x02, 0x58, 0x04, 0x00]);
        jpeg.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let jpeg = base64_encode(&jpeg);
        assert_eq!(
            get_jpeg_dimensions(&jpeg),
            Some(ImageDimensions {
                width_px: 1024,
                height_px: 600
            })
        );
        assert_eq!(get_jpeg_dimensions("AAAA"), None);

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x40, 0x00, 0x20, 0x00, 0, 0, 0]);
        let gif = base64_encode(&gif);
        assert_eq!(
            get_gif_dimensions(&gif),
            Some(ImageDimensions {
                width_px: 64,
                height_px: 32
            })
        );
        assert_eq!(get_gif_dimensions("AAAA"), None);

        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"VP8 ");
        webp.extend_from_slice(&[0; 10]);
        webp.extend_from_slice(&[0x40, 0x01, 0x00, 0x02]);
        let webp = base64_encode(&webp);
        assert_eq!(
            get_webp_dimensions(&webp),
            Some(ImageDimensions {
                width_px: 320,
                height_px: 512
            })
        );
        assert_eq!(get_webp_dimensions("AAAA"), None);
    }

    #[test]
    fn boolean_override_parsing() {
        assert_eq!(parse_boolean_capability_override(Some("1")), Some(true));
        assert_eq!(parse_boolean_capability_override(Some("0")), Some(false));
        assert_eq!(parse_boolean_capability_override(Some("auto")), None);
        assert_eq!(parse_boolean_capability_override(None), None);
    }

    #[test]
    fn shorten_image_path_matches_home_prefixes() {
        assert_eq!(
            shorten_image_path("/home/u/a.png", Some("/home/u")),
            "~/a.png"
        );
        assert_eq!(shorten_image_path("/home/u", Some("/home/u")), "~");
        assert_eq!(
            shorten_image_path("/home/user/a.png", Some("/home/u")),
            "/home/user/a.png"
        );
        assert_eq!(shorten_image_path("C:\\a.png", Some("C:")), "~\\a.png");
        assert_eq!(shorten_image_path("relative.png", None), "relative.png");
    }
}

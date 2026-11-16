//! Fullscreen 3D easter eggs: the pi logo (header logo click) and Armin
//! (`/arminsayshi`). Both are bitmaps built from one block per pixel. The
//! current screen dissolves into braille dust while the model spins in the
//! center and its blocks play a sliding puzzle. Leaving plays the same
//! timeline backwards, so the screen reassembles.
//!
//! Port of `components/easter-egg-3d.js`. The pi logo lifts off the header,
//! flies to the center, and grows; the dust spreads out from the header
//! logo. Armin grows out of a speck at the center; the dust spreads out from
//! the center. The backwards exit runs over `EXIT_DURATION` (1.1s): `close()`
//! `:916-935` snapshots the exit timeline, the render pass applies it
//! `:962-981` (reverse dissolve, gathered offsets, yaw eased to the front
//! view, hint/star alpha fading), and `exitProgress` `:1114-1118` reaches 1
//! to finish (`:1120-1125`).
//!
//! The blocks are ray cast per braille dot. A braille cell holds 2x4 roughly
//! square dots, so one pixel of a half-block bitmap is exactly 2x2 dots, and
//! the header logo (4x2 cells) is 8x8 dots when it lifts off.
//!
//! Deviations from upstream, all host-shape driven:
//! - Upstream drives frames with a `setInterval` and calls
//!   `tui.requestRender()`; this port has no timers — the interactive loop
//!   renders every pass, so the animation computes its pose from elapsed
//!   time and the terminal's row count arrives through a cell the host
//!   refreshes each pass (as fresh as upstream's 30fps read).
//! - Upstream awaits a fresh `queryTerminalColors` before fading; the port
//!   reads the colors the startup query already reported (the same terminal
//!   defaults, without the async round-trip).
//! - Upstream's animation stops itself when it goes a second without being
//!   rendered (pi hides overlays on exit); the port's host drops the
//!   overlay, so the check has no equivalent to port.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pi_tui::color::{background_ansi, color_to_rgb, foreground_ansi, indexed_color, rgb_color};
use pi_tui::component::Component;
use pi_tui::keybindings::{self, action};
use pi_tui::mouse::{MouseEvent, MouseEventKind, MouseEventResult};
use pi_tui::oklab::Rgb;
use pi_tui::theme::{ThemeAppearance, ThemeColor};
use pi_tui::tui::{OverlayAnchor, OverlayId, OverlayOptions, SizeValue, Tui};

use super::super::theme;
use super::armin::{ARMIN_HEIGHT, ARMIN_WIDTH, is_armin_pixel};

const DEPTH: f64 = 0.7;
const FLY_START: f64 = 0.1;
const FLY_DURATION: f64 = 1.3;
/// Braille dots per pixel when a model without an origin appears, growing
/// from a speck at the center.
const START_SCALE: f64 = 0.1;

// The sliding puzzle: after the model spun for a while, blocks slide into
// free neighboring cells, a few at a time. Each cycle shuffles, flies every
// block back home, and holds the model briefly.
const PUZZLE_START: f64 = FLY_START + FLY_DURATION + 3.0;
const PUZZLE_STEP: f64 = 0.3;
const PUZZLE_STEPS: u64 = 12;
const PUZZLE_RETURN: f64 = 1.0;
const PUZZLE_HOLD: f64 = 0.6;
const PUZZLE_CYCLE: f64 = PUZZLE_STEP * PUZZLE_STEPS as f64 + PUZZLE_RETURN + PUZZLE_HOLD;
const PUZZLE_MOVES: [[i64; 3]; 10] = [
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [0, 0, 1],
    [0, 0, -1],
];

const DUST_DURATION: f64 = 0.35;
const WAVE_SPREAD: f64 = 0.55;
const WAVE_JITTER: f64 = 0.12;
const DISSOLVE_END: f64 = FLY_START + WAVE_SPREAD + WAVE_JITTER + DUST_DURATION;
const BACKGROUND_FADE: f64 = 0.5;
const EXIT_DURATION: f64 = 1.1;
// A faint starfield fades in behind the logo once the screen has dissolved.
const STARS_START: f64 = 2.5;
const STARS_FADE: f64 = 2.0;
const STAR_DENSITY: f64 = 0.018;
// Star tints: neutral, pale blue, pale gold, and pale rose, each mixed half
// with the terminal's foreground.
const STAR_TINTS: [[f64; 3]; 4] = [
    [255.0, 255.0, 255.0],
    [170.0, 195.0, 255.0],
    [255.0, 225.0, 170.0],
    [255.0, 190.0, 205.0],
];
const LOGO_COLOR_STEP: f64 = 5.0;
// On light backgrounds most of each braille cell shows the bright
// background, which washes the logo out. There, the logo gets a halo: its
// colors and coverage blurred into the cell backgrounds, strongest under the
// logo and fading out over HALO_RADIUS_X columns and HALO_RADIUS_Y rows
// (cells are about twice as tall as wide).
const LIGHT_HALO_STRENGTH: f64 = 0.3;
const HALO_RADIUS_X: usize = 6;
const HALO_RADIUS_Y: usize = 3;
const HALO_LEVELS: f64 = 32.0;
const STAR_ALPHA_LEVELS: f64 = 8.0;
const STAR_FLICKER_LEVELS: f64 = 3.0;
const HINT_FADE: f64 = 0.5;

/// Braille dot bits, indexed by row * 2 + column within the 2x4 cell.
const DOT_BITS: [u8; 8] = [0x01, 0x08, 0x02, 0x10, 0x04, 0x20, 0x40, 0x80];

// normalize([-0.45, -0.6, 0.75]) and normalize(LIGHT + [0, 0, 1]), folded to
// literals because const fns cannot call `sqrt`.
const LIGHT: [f64; 3] = [
    -0.424_264_068_711_928_5,
    -0.565_685_424_949_238,
    std::f64::consts::FRAC_1_SQRT_2,
];
const HALF_VECTOR: [f64; 3] = [
    -0.229_610_059_419_053_9,
    -0.306_146_745_892_071_8,
    0.923_879_532_511_286_7,
];

// The spin is locked to the puzzle: one turn per cycle, facing the camera in
// the middle of each hold.
const SPIN_RAMP: f64 = 1.4;
const SPIN_SPEED: f64 = std::f64::consts::TAU / PUZZLE_CYCLE;
// How much the spin slows down while facing the camera (and speeds up while
// facing away), from 0 to 1.
const SPIN_LINGER: f64 = 0.6;
const FIRST_FRONT_VIEW: f64 =
    PUZZLE_START + PUZZLE_STEP * PUZZLE_STEPS as f64 + PUZZLE_RETURN + PUZZLE_HOLD / 2.0;

const CORAL: [f64; 3] = [228.0, 138.0, 122.0];
const BLUE: [f64; 3] = [79.0, 142.0, 179.0];
const YELLOW: [f64; 3] = [234.0, 182.0, 93.0];
const PI_LOGO_PIXELS: [&str; 4] = ["ccc.", "b.c.", "bb.y", "b..y"];

/// Which easter egg to play. The pi logo lifts off the header logo, whose
/// top-left cell is at `column`, `row`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EasterEgg3d {
    PiLogo { column: usize, row: usize },
    Armin,
}

/// How many blocks the puzzle moves per step, given a random number from 0
/// to 1 (`Model.puzzleMoves`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PuzzleMoveCount {
    Logo,
    Armin,
}

impl PuzzleMoveCount {
    fn count(self, random: f64, block_count: usize) -> usize {
        match self {
            PuzzleMoveCount::Logo => {
                if random < 0.4 {
                    2
                } else {
                    1
                }
            }
            PuzzleMoveCount::Armin => {
                (block_count as f64 * (0.03 + random * 0.04)).round() as usize
            }
        }
    }
}

struct Block {
    /// Grid position: column, row, and layer (-1, 0, 1) in depth.
    home: [i64; 3],
    color: [f64; 3],
}

/// A bitmap built from one block per foreground pixel. Each is a block that
/// the puzzle slides around (`Model`).
struct Model {
    /// Bitmap size in pixels.
    columns: usize,
    rows: usize,
    blocks: Vec<Block>,
    /// Camera distance from the model's center, in pixels.
    camera_distance: f64,
    /// Farthest distance of any block corner from the center, with blocks on
    /// the outer depth layers.
    radius: f64,
    /// Largest share of the screen width the spinning model covers.
    width_share: f64,
    puzzle_moves: PuzzleMoveCount,
    /// Top-left cell of the model's half-block rendering on screen; the
    /// model lifts off from there and replaces it. `None` to grow out of the
    /// center.
    origin: Option<(usize, usize)>,
}

fn create_model(
    columns: usize,
    rows: usize,
    pixel: impl Fn(usize, usize) -> Option<[f64; 3]>,
    camera_distance: f64,
    width_share: f64,
    puzzle_moves: PuzzleMoveCount,
    origin: Option<(usize, usize)>,
) -> Model {
    let mut blocks = Vec::new();
    for row in 0..rows {
        for column in 0..columns {
            if let Some(color) = pixel(column, row) {
                blocks.push(Block {
                    home: [column as i64, row as i64, 0],
                    color,
                });
            }
        }
    }
    Model {
        columns,
        rows,
        radius: (columns as f64 / 2.0)
            .hypot(rows as f64 / 2.0)
            .hypot(1.0 + DEPTH / 2.0),
        blocks,
        camera_distance,
        width_share,
        puzzle_moves,
        origin,
    }
}

fn pi_logo_model(origin: Option<(usize, usize)>) -> Model {
    create_model(
        4,
        4,
        |column, row| {
            let pixels = PI_LOGO_PIXELS.get(row)?;
            match pixels.as_bytes().get(column) {
                Some(b'c') => Some(CORAL),
                Some(b'b') => Some(BLUE),
                Some(b'y') => Some(YELLOW),
                _ => None,
            }
        },
        10.0,
        0.35,
        PuzzleMoveCount::Logo,
        origin,
    )
}

fn armin_model(color: [f64; 3]) -> Model {
    create_model(
        ARMIN_WIDTH,
        ARMIN_HEIGHT,
        |column, row| is_armin_pixel(column, row).then_some(color),
        // Far enough that the perspective stays mild for a figure about 36
        // blocks tall.
        80.0,
        0.45,
        PuzzleMoveCount::Armin,
        None,
    )
}

fn clamp01(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

/// Quintic smoothstep.
fn smooth(value: f64) -> f64 {
    let t = clamp01(value);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn ease_out(value: f64) -> f64 {
    let t = 1.0 - clamp01(value);
    1.0 - t * t * t
}

fn mix(a: [f64; 3], b: [f64; 3], amount: f64) -> [f64; 3] {
    [
        a[0] + (b[0] - a[0]) * amount,
        a[1] + (b[1] - a[1]) * amount,
        a[2] + (b[2] - a[2]) * amount,
    ]
}

/// The 32-bit scramble JS reaches for with `Math.imul` shifts; returns a
/// float in `0..1`.
fn hash(value: u64) -> f64 {
    let mut x = (value as u32) ^ 0x9e37_79b9;
    x = x.wrapping_mul(0x85eb_ca6b);
    x ^= x >> 13;
    x = x.wrapping_mul(0xc2b2_ae35);
    x ^= x >> 16;
    f64::from(x) / 4_294_967_296.0
}

/// Whether a background is light, by relative luminance.
fn is_light(background: [f64; 3]) -> bool {
    0.2126 * background[0] + 0.7152 * background[1] + 0.0722 * background[2] > 128.0
}

/// Braille dots a glyph turns into, a rough measure of how much ink it has.
fn glyph_ink(text: &str, seed: f64) -> u32 {
    if text.trim().is_empty() {
        return 0;
    }
    let mut chars = text.chars();
    let (Some(single), None) = (chars.next(), chars.next()) else {
        return 4 + (seed * 3.0) as u32;
    };
    if matches!(single, '.' | ',' | ':' | ';' | '\'' | '`' | '-' | '_' | '·') {
        2
    } else if ('\u{2500}'..='\u{257f}').contains(&single) {
        3
    } else {
        4 + (seed * 3.0) as u32
    }
}

/// A parsed screen cell with resolved colors (`parseScreen`'s cell).
struct ScreenCell {
    text: String,
    width: usize,
    fg: [f64; 3],
    bg: Option<[f64; 3]>,
    delay: f64,
    ink: u32,
    seed: u64,
}

/// Parse rendered lines into cells with resolved colors. Other escape
/// sequences (OSC, APC, cursor) are skipped (`parseScreen`).
#[allow(clippy::too_many_lines)] // mirrors the TS `parseScreen`; split when extending
fn parse_screen(
    lines: &[String],
    width: usize,
    foreground: [f64; 3],
    background: [f64; 3],
) -> Vec<Vec<ScreenCell>> {
    let palette = |index: u16| -> [f64; 3] {
        let Rgb { r, g, b } = color_to_rgb(indexed_color(index));
        [r, g, b]
    };
    lines
        .iter()
        .map(|line| {
            let mut cells: Vec<ScreenCell> = Vec::new();
            let mut fg: Option<[f64; 3]> = None;
            let mut bg: Option<[f64; 3]> = None;
            let mut dim = false;
            let mut inverse = false;
            // The SGR walker mutates the four state variables above, so it
            // takes them as `&mut` through a closure.
            macro_rules! apply_sgr {
                ($params:expr) => {{
                    let params = $params;
                    let codes: Vec<&str> = if params.is_empty() {
                        vec!["0"]
                    } else {
                        params.split(';').collect()
                    };
                    let mut i = 0;
                    while i < codes.len() {
                        let parts: Vec<Option<u32>> = codes[i]
                            .split(':')
                            .map(|part| part.parse::<u32>().ok())
                            .collect();
                        let code = parts[0].unwrap_or(0);
                        if code == 38 || code == 48 {
                            // Extended color, either as `38;5;n` / `38;2;r;g;b`
                            // or colon sub-parameters.
                            let args: Vec<u32> = if parts.len() > 1 {
                                parts[1..].iter().filter_map(|part| *part).collect()
                                // `38:2::r:g:b` has an empty color space id,
                                // dropped above.
                            } else {
                                let kind = codes
                                    .get(i + 1)
                                    .and_then(|part| part.parse::<u32>().ok())
                                    .unwrap_or(u32::MAX);
                                let count = match kind {
                                    5 => 2,
                                    2 => 4,
                                    _ => 1,
                                };
                                let taken: Vec<u32> = codes[i + 1..i + 1 + count]
                                    .iter()
                                    .filter_map(|part| part.parse::<u32>().ok())
                                    .collect();
                                i += count;
                                taken
                            };
                            let color = if args.first() == Some(&5) {
                                args.get(1).map(|index| palette(*index as u16))
                            } else if args.first() == Some(&2) && args.len() >= 4 {
                                Some([f64::from(args[1]), f64::from(args[2]), f64::from(args[3])])
                            } else {
                                None
                            };
                            if let Some(color) = color {
                                if code == 38 {
                                    fg = Some(color);
                                } else {
                                    bg = Some(color);
                                }
                            }
                        } else if code == 0 {
                            fg = None;
                            bg = None;
                            dim = false;
                            inverse = false;
                        } else if code == 2 {
                            dim = true;
                        } else if code == 22 {
                            dim = false;
                        } else if code == 7 {
                            inverse = true;
                        } else if code == 27 {
                            inverse = false;
                        } else if (30..=37).contains(&code) {
                            fg = Some(palette((code - 30) as u16));
                        } else if (90..=97).contains(&code) {
                            fg = Some(palette((code - 90 + 8) as u16));
                        } else if code == 39 {
                            fg = None;
                        } else if (40..=47).contains(&code) {
                            bg = Some(palette((code - 40) as u16));
                        } else if (100..=107).contains(&code) {
                            bg = Some(palette((code - 100 + 8) as u16));
                        } else if code == 49 {
                            bg = None;
                        }
                        i += 1;
                    }
                }};
            }
            // Push one segment's text, resolving the active SGR state per
            // cell (`pushText`).
            macro_rules! push_text {
                ($text:expr) => {{
                    for char in $text.chars() {
                        let glyph_width = pi_tui::ansi::visible_width(&char.to_string());
                        if glyph_width == 0 || cells.len() + glyph_width > width {
                            continue;
                        }
                        let mut cell_fg = fg.unwrap_or(foreground);
                        let mut cell_bg = bg;
                        if inverse {
                            cell_fg = bg.unwrap_or(background);
                            cell_bg = Some(fg.unwrap_or(foreground));
                        }
                        if dim {
                            cell_fg = mix(cell_fg, cell_bg.unwrap_or(background), 0.4);
                        }
                        cells.push(ScreenCell {
                            text: char.to_string(),
                            width: glyph_width,
                            fg: cell_fg,
                            bg: cell_bg,
                            delay: 0.0,
                            ink: 0,
                            seed: 0,
                        });
                        if glyph_width == 2 {
                            // Wide graphemes share their first column's state.
                            cells.push(ScreenCell {
                                text: String::new(),
                                width: 0,
                                fg: cell_fg,
                                bg: cell_bg,
                                delay: 0.0,
                                ink: 0,
                                seed: 0,
                            });
                        }
                    }
                }};
            }
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < line.len() {
                let Some(sequence_start) = line[i..].find('\x1b').map(|offset| i + offset) else {
                    push_text!(&line[i..]);
                    break;
                };
                if sequence_start > i {
                    push_text!(&line[i..sequence_start]);
                }
                let kind = bytes.get(sequence_start + 1).copied();
                match kind {
                    Some(b'[') => {
                        let mut end = sequence_start + 2;
                        while end < line.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                            end += 1;
                        }
                        if bytes.get(end) == Some(&b'm') {
                            apply_sgr!(&line[sequence_start + 2..end]);
                        }
                        i = end + 1;
                    }
                    Some(b']' | b'_' | b'P' | b'^') => {
                        // String sequences end at BEL or ST (ESC \).
                        let bell = line[sequence_start + 2..].find('\x07');
                        let st = line[sequence_start + 2..].find("\x1b\\");
                        match (bell, st) {
                            (None, None) => break,
                            (Some(bell), Some(st)) => {
                                i = if bell < st {
                                    sequence_start + 2 + bell + 1
                                } else {
                                    sequence_start + 2 + st + 2
                                };
                            }
                            (Some(bell), None) => i = sequence_start + 2 + bell + 1,
                            (None, Some(st)) => i = sequence_start + 2 + st + 2,
                        }
                    }
                    _ => {
                        i = sequence_start + 2;
                    }
                }
            }
            cells
        })
        .collect()
}

/// Row-major 3x3 rotation matrix for yaw (y), then pitch (x), then roll (z)
/// (`rotation`): Rz * Rx * Ry.
fn rotation(yaw: f64, pitch: f64, roll: f64) -> [f64; 9] {
    let (sy, cy) = (yaw.sin(), yaw.cos());
    let (sx, cx) = (pitch.sin(), pitch.cos());
    let (sz, cz) = (roll.sin(), roll.cos());
    let ry: [f64; 9] = [cy, 0.0, sy, 0.0, 1.0, 0.0, -sy, 0.0, cy];
    let rx: [f64; 9] = [1.0, 0.0, 0.0, 0.0, cx, -sx, 0.0, sx, cx];
    let rz: [f64; 9] = [cz, -sz, 0.0, sz, cz, 0.0, 0.0, 0.0, 1.0];
    multiply(&rz, &multiply(&rx, &ry))
}

fn multiply(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut result = [0.0; 9];
    for row in 0..3 {
        for column in 0..3 {
            result[row * 3 + column] = a[row * 3] * b[column]
                + a[row * 3 + 1] * b[3 + column]
                + a[row * 3 + 2] * b[6 + column];
        }
    }
    result
}

/// Uniform spin that accelerates from rest over `SPIN_RAMP`, then turns at
/// `SPIN_SPEED` (`baseSpinPhase`).
fn base_spin_phase(time: f64) -> f64 {
    let elapsed = time - FLY_START;
    if elapsed <= 0.0 {
        return 0.0;
    }
    let u = elapsed / SPIN_RAMP;
    // Integral of the quintic smoothstep ramp.
    if u < 1.0 {
        SPIN_SPEED * SPIN_RAMP * (u.powi(6) - 3.0 * u.powi(5) + 2.5 * u.powi(4))
    } else {
        SPIN_SPEED * (SPIN_RAMP * 0.5 + elapsed - SPIN_RAMP)
    }
}

/// Extra rotation added during the flight, so the phase is a whole number of
/// turns at every assembled front view (`SPIN_ALIGNMENT`).
fn spin_alignment() -> f64 {
    std::f64::consts::TAU - (base_spin_phase(FIRST_FRONT_VIEW) % std::f64::consts::TAU)
}

/// Spin phase: whole turns exactly when the reassembled logo faces the
/// camera (`spinPhase`).
fn spin_phase(time: f64) -> f64 {
    base_spin_phase(time) + spin_alignment() * smooth((time - FLY_START) / FLY_DURATION)
}

/// Yaw of the logo. It matches the phase at whole turns but lingers there,
/// so the logo reads from the front (`spinAngle`).
fn spin_angle(time: f64) -> f64 {
    let phase = spin_phase(time);
    phase - SPIN_LINGER * phase.sin()
}

/// Block positions after each step of one shuffle cycle, starting at home.
/// Deterministic per cycle (`shuffleSteps`).
fn shuffle_steps(model: &Model, cycle: u64) -> Vec<Vec<[i64; 3]>> {
    let mut positions: Vec<[i64; 3]> = model.blocks.iter().map(|block| block.home).collect();
    let mut steps = vec![positions.clone()];
    let mut last_moved: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut random = cycle.wrapping_mul(7_919).wrapping_add(1);
    let mut next = move || {
        let value = hash(random);
        random = random.wrapping_add(1);
        value
    };
    let key = |position: [i64; 3]| -> i64 {
        ((position[2] + 1) * model.rows as i64 + position[1]) * model.columns as i64 + position[0]
    };
    for _ in 0..PUZZLE_STEPS {
        let mut occupied: std::collections::HashSet<i64> =
            positions.iter().map(|position| key(*position)).collect();
        let mut next_positions = positions.clone();
        let mut moved: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let move_count = model.puzzle_moves.count(next(), model.blocks.len());
        for _ in 0..move_count {
            let mut candidates: Vec<(usize, [i64; 3])> = Vec::new();
            for (block, position) in positions.iter().enumerate() {
                if moved.contains(&block) {
                    continue;
                }
                let [x, y, z] = *position;
                for [dx, dy, dz] in PUZZLE_MOVES {
                    let target = [x + dx, y + dy, z + dz];
                    if target[0] < 0
                        || target[0] >= model.columns as i64
                        || target[1] < 0
                        || target[1] >= model.rows as i64
                    {
                        continue;
                    }
                    if target[2] < -1 || target[2] > 1 || occupied.contains(&key(target)) {
                        continue;
                    }
                    candidates.push((block, target));
                }
            }
            // Prefer blocks that did not just move, so the puzzle does not
            // look like one block jittering.
            let fresh: Vec<(usize, [i64; 3])> = candidates
                .iter()
                .filter(|(block, _)| !last_moved.contains(block))
                .copied()
                .collect();
            let pool: Vec<(usize, [i64; 3])> = if fresh.is_empty() { candidates } else { fresh };
            let choice = (next() * pool.len() as f64) as usize;
            let Some(&(block, target)) = pool.get(choice) else {
                break;
            };
            occupied.insert(key(target));
            next_positions[block] = target;
            moved.insert(block);
        }
        last_moved.clear();
        last_moved.extend(moved.iter().copied());
        positions = next_positions;
        steps.push(positions.clone());
    }
    steps
}

/// A box in model space: axis-aligned bounds plus its block color.
struct Box3 {
    min: [f64; 3],
    max: [f64; 3],
    color: [f64; 3],
}

/// One renderable face of a box, with its precomputed lighting.
struct Face {
    axis: usize,
    plane: f64,
    u_min: f64,
    u_max: f64,
    v_min: f64,
    v_max: f64,
    min_x: usize,
    min_y: usize,
    max_x: usize,
    max_y: usize,
    red: f64,
    green: f64,
    blue: f64,
}

/// The camera-space pose of the model (`pose`'s return value).
struct Pose {
    center_x: f64,
    center_y: f64,
    scale: f64,
    yaw: f64,
    pitch: f64,
    roll: f64,
}

/// A cleared-rectangle bookkeeping entry for the incremental buffers.
#[derive(Debug, Clone, Copy)]
struct DirtyRect {
    min_x: usize,
    min_y: usize,
    max_x: usize,
    max_y: usize,
}

/// Renders the model's blocks into braille cells. Only faces that point at
/// the camera and are not covered by a touching block are drawn. Each face
/// is rasterized over its projected bounds by intersecting each dot's ray
/// with the face's plane, with a depth buffer resolving overlaps. Buffers
/// are reused between frames (`BlockRaster`).
struct BlockRaster {
    /// Braille dot bits per cell.
    bits: Vec<u8>,
    /// Lit dots per cell.
    counts: Vec<u8>,
    /// Summed RGB of the lit dots per cell.
    rgb: Vec<f32>,
    /// Halo tint strength per cell, from 0 to 1. Only set when a halo was
    /// requested.
    halo_amount: Vec<f32>,
    /// Halo color per cell.
    halo_rgb: Vec<f32>,
    width: usize,
    height: usize,
    /// Ray parameter of the nearest hit per dot; smaller is nearer.
    depth: Vec<f32>,
    /// Face index + 1 of the nearest hit per dot, 0 for none.
    face_ids: Vec<u16>,
    /// Cells written by the previous frame, cleared before the next one.
    dirty: Option<DirtyRect>,
    /// Cells with a halo from the previous frame, cleared before the next.
    halo_dirty: Option<DirtyRect>,
    camera_distance: f64,
}

impl BlockRaster {
    fn new(camera_distance: f64) -> Self {
        Self {
            bits: Vec::new(),
            counts: Vec::new(),
            rgb: Vec::new(),
            halo_amount: Vec::new(),
            halo_rgb: Vec::new(),
            width: 0,
            height: 0,
            depth: Vec::new(),
            face_ids: Vec::new(),
            dirty: None,
            halo_dirty: None,
            camera_distance,
        }
    }

    #[allow(clippy::too_many_lines)] // the TS frame pipeline in one pass; split when extending
    fn render(
        &mut self,
        width: usize,
        height: usize,
        pose: &Pose,
        boxes: &[Box3],
        background: [f64; 3],
        halo_strength: f64,
    ) {
        let dot_width = width * 2;
        let dot_height = height * 4;
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.bits = vec![0; width * height];
            self.counts = vec![0; width * height];
            self.rgb = vec![0.0; width * height * 3];
            self.halo_amount = vec![0.0; width * height];
            self.halo_rgb = vec![0.0; width * height * 3];
            self.halo_dirty = None;
            self.depth = vec![f32::INFINITY; dot_width * dot_height];
            self.face_ids = vec![0; dot_width * dot_height];
            self.dirty = None;
        } else if let Some(dirty) = self.dirty {
            for row in dirty.min_y..=dirty.max_y {
                let cell_row = row * width;
                self.bits[cell_row + dirty.min_x..=cell_row + dirty.max_x].fill(0);
                self.counts[cell_row + dirty.min_x..=cell_row + dirty.max_x].fill(0);
                let rgb_row = cell_row * 3;
                self.rgb[rgb_row + dirty.min_x * 3..rgb_row + (dirty.max_x + 1) * 3].fill(0.0);
            }
            self.dirty = None;
        }
        if let Some(dirty) = self.halo_dirty {
            for row in dirty.min_y..=dirty.max_y {
                let cell_row = row * width;
                self.halo_amount[cell_row + dirty.min_x..=cell_row + dirty.max_x].fill(0.0);
                let rgb_row = cell_row * 3;
                self.halo_rgb[rgb_row + dirty.min_x * 3..rgb_row + (dirty.max_x + 1) * 3].fill(0.0);
            }
            self.halo_dirty = None;
        }
        let matrix = rotation(pose.yaw, pose.pitch, pose.roll);
        let camera_distance = self.camera_distance;
        // The camera sits at (0, 0, cameraDistance) in camera space; object
        // space is the transpose rotation.
        let origin = [
            matrix[6] * camera_distance,
            matrix[7] * camera_distance,
            matrix[8] * camera_distance,
        ];
        let light = is_light(background);
        let faces = self.visible_faces(&matrix, origin, pose, boxes, dot_width, dot_height, light);
        if faces.is_empty() {
            return;
        }
        let mut min_x = dot_width;
        let mut min_y = dot_height;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut any = false;
        for face in &faces {
            min_x = min_x.min(face.min_x);
            min_y = min_y.min(face.min_y);
            max_x = max_x.max(face.max_x);
            max_y = max_y.max(face.max_y);
            any = true;
        }
        if !any || max_x < min_x || max_y < min_y {
            return;
        }
        for (face_index, face) in faces.iter().enumerate() {
            let axis = face.axis;
            let u = (axis + 1) % 3;
            let v = (axis + 2) % 3;
            let origin_axis = origin[axis];
            let origin_u = origin[u];
            let origin_v = origin[v];
            // The ray direction in object space is linear in the dot
            // position, so it is stepped per dot.
            let step_axis = matrix[axis] / pose.scale;
            let step_u = matrix[u] / pose.scale;
            let step_v = matrix[v] / pose.scale;
            let sx = (face.min_x as f64 + 0.5 - pose.center_x) / pose.scale;
            for dot_y in face.min_y..=face.max_y {
                let sy = (dot_y as f64 + 0.5 - pose.center_y) / pose.scale;
                let mut direction_axis =
                    matrix[axis] * sx + matrix[3 + axis] * sy - matrix[6 + axis] * camera_distance;
                let mut direction_u =
                    matrix[u] * sx + matrix[3 + u] * sy - matrix[6 + u] * camera_distance;
                let mut direction_v =
                    matrix[v] * sx + matrix[3 + v] * sy - matrix[6 + v] * camera_distance;
                let row_base = dot_y * dot_width;
                for index in row_base + face.min_x..=row_base + face.max_x {
                    let t = (face.plane - origin_axis) / direction_axis;
                    if t > 0.0 && (t as f32) < self.depth[index] {
                        let hit_u = origin_u + t * direction_u;
                        let hit_v = origin_v + t * direction_v;
                        if (face.u_min..=face.u_max).contains(&hit_u)
                            && (face.v_min..=face.v_max).contains(&hit_v)
                        {
                            self.depth[index] = t as f32;
                            self.face_ids[index] = (face_index + 1) as u16;
                        }
                    }
                    direction_axis += step_axis;
                    direction_u += step_u;
                    direction_v += step_v;
                }
            }
        }
        // Shade the hit dots, pack them into braille cells, and reset the
        // dot buffers for the next frame.
        let mut cell_min_x = width;
        let mut cell_min_y = height;
        let mut cell_max_x = 0usize;
        let mut cell_max_y = 0usize;
        let mut touched = false;
        for dot_y in min_y..=max_y {
            let mut index = dot_y * dot_width + min_x;
            for dot_x in min_x..=max_x {
                let id = self.face_ids[index];
                if id == 0 {
                    index += 1;
                    continue;
                }
                let face = &faces[id as usize - 1];
                // Points farther from the camera fade slightly toward the
                // background for depth.
                let fog = clamp01(0.15 - camera_distance * (1.0 - self.depth[index] as f64) * 0.12)
                    * if light { 0.5 } else { 1.0 };
                self.face_ids[index] = 0;
                self.depth[index] = f32::INFINITY;
                let cell_x = dot_x >> 1;
                let cell_y = dot_y >> 2;
                let cell = cell_y * width + cell_x;
                self.bits[cell] |= DOT_BITS[(dot_y & 3) * 2 + (dot_x & 1)];
                self.counts[cell] = self.counts[cell].saturating_add(1);
                self.rgb[cell * 3] += (face.red + (background[0] - face.red) * fog) as f32;
                self.rgb[cell * 3 + 1] += (face.green + (background[1] - face.green) * fog) as f32;
                self.rgb[cell * 3 + 2] += (face.blue + (background[2] - face.blue) * fog) as f32;
                cell_min_x = cell_min_x.min(cell_x);
                cell_max_x = cell_max_x.max(cell_x);
                cell_min_y = cell_min_y.min(cell_y);
                cell_max_y = cell_max_y.max(cell_y);
                touched = true;
                index += 1;
                let _ = dot_x;
            }
        }
        if touched {
            self.dirty = Some(DirtyRect {
                min_x: cell_min_x,
                min_y: cell_min_y,
                max_x: cell_max_x,
                max_y: cell_max_y,
            });
        }
        if halo_strength > 0.0 {
            self.render_halo(halo_strength);
        }
    }

    /// Blur the logo's coverage and color over neighboring cells with a
    /// separable tent filter, so the tint falls off smoothly past the logo's
    /// edges (`renderHalo`).
    fn render_halo(&mut self, strength: f64) {
        let Some(cells) = self.dirty else {
            return;
        };
        let width = self.width;
        let height = self.height;
        let min_x = cells.min_x.saturating_sub(HALO_RADIUS_X);
        let max_x = (cells.max_x + HALO_RADIUS_X).min(width - 1);
        let min_y = cells.min_y.saturating_sub(HALO_RADIUS_Y);
        let max_y = (cells.max_y + HALO_RADIUS_Y).min(height - 1);
        let region_width = max_x - min_x + 1;
        let tent = |radius: usize| -> Vec<f64> {
            let weights: Vec<f64> = (0..=radius * 2)
                .map(|i| (radius + 1 - i.abs_diff(radius)) as f64)
                .collect();
            let total: f64 = weights.iter().sum();
            weights.into_iter().map(|weight| weight / total).collect()
        };
        let weights_x = tent(HALO_RADIUS_X);
        let weights_y = tent(HALO_RADIUS_Y);
        // Horizontal pass over the rows that contain the logo: coverage and
        // premultiplied color per cell.
        let rows = cells.max_y - cells.min_y + 1;
        let mut horizontal = vec![0.0f32; rows * region_width * 4];
        for y in cells.min_y..=cells.max_y {
            for x in min_x..=max_x {
                let mut coverage = 0.0f64;
                let mut red = 0.0f64;
                let mut green = 0.0f64;
                let mut blue = 0.0f64;
                for (dx, weight) in weights_x.iter().enumerate() {
                    let source_x = (x as isize - HALO_RADIUS_X as isize + dx as isize) as usize;
                    if !(cells.min_x..=cells.max_x).contains(&source_x) {
                        continue;
                    }
                    let source = y * width + source_x;
                    let count = self.counts[source];
                    if count == 0 {
                        continue;
                    }
                    let weight = *weight / 8.0;
                    coverage += f64::from(count) * weight;
                    red += f64::from(self.rgb[source * 3]) * weight;
                    green += f64::from(self.rgb[source * 3 + 1]) * weight;
                    blue += f64::from(self.rgb[source * 3 + 2]) * weight;
                }
                let target = ((y - cells.min_y) * region_width + (x - min_x)) * 4;
                horizontal[target] = coverage as f32;
                horizontal[target + 1] = red as f32;
                horizontal[target + 2] = green as f32;
                horizontal[target + 3] = blue as f32;
            }
        }
        // Vertical pass into the halo buffers.
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let mut coverage = 0.0f64;
                let mut red = 0.0f64;
                let mut green = 0.0f64;
                let mut blue = 0.0f64;
                for (dy, weight) in weights_y.iter().enumerate() {
                    let source_y = y as isize - HALO_RADIUS_Y as isize + dy as isize;
                    if !(cells.min_y..=cells.max_y).contains(&(source_y as usize)) {
                        continue;
                    }
                    let weight = *weight;
                    let source =
                        ((source_y as usize - cells.min_y) * region_width + (x - min_x)) * 4;
                    coverage += f64::from(horizontal[source]) * weight;
                    red += f64::from(horizontal[source + 1]) * weight;
                    green += f64::from(horizontal[source + 2]) * weight;
                    blue += f64::from(horizontal[source + 3]) * weight;
                }
                let target = y * width + x;
                // Cells under the logo keep at least their own coverage, so
                // faces stay solidly tinted.
                let count = self.counts[target];
                if count > 0 && f64::from(count) / 8.0 > coverage {
                    coverage = f64::from(count) / 8.0;
                    red = f64::from(self.rgb[target * 3]) / f64::from(count) * coverage;
                    green = f64::from(self.rgb[target * 3 + 1]) / f64::from(count) * coverage;
                    blue = f64::from(self.rgb[target * 3 + 2]) / f64::from(count) * coverage;
                }
                if coverage <= 0.0 {
                    continue;
                }
                // Quantized so neighboring cells usually share a background
                // and its escape sequence.
                let amount =
                    (strength * clamp01(coverage).powf(0.7) * HALO_LEVELS).round() / HALO_LEVELS;
                if amount <= 0.0 {
                    continue;
                }
                self.halo_amount[target] = amount as f32;
                self.halo_rgb[target * 3] =
                    ((red / coverage / LOGO_COLOR_STEP).round() * LOGO_COLOR_STEP) as f32;
                self.halo_rgb[target * 3 + 1] =
                    ((green / coverage / LOGO_COLOR_STEP).round() * LOGO_COLOR_STEP) as f32;
                self.halo_rgb[target * 3 + 2] =
                    ((blue / coverage / LOGO_COLOR_STEP).round() * LOGO_COLOR_STEP) as f32;
            }
        }
        self.halo_dirty = Some(DirtyRect {
            min_x,
            min_y,
            max_x,
            max_y,
        });
    }

    /// The faces that point at the camera and are not covered by a touching
    /// block, with their projected dot bounds and lighting
    /// (`visibleFaces`).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)] // mirrors the TS `visibleFaces`; split when extending
    fn visible_faces(
        &self,
        matrix: &[f64; 9],
        origin: [f64; 3],
        pose: &Pose,
        boxes: &[Box3],
        dot_width: usize,
        dot_height: usize,
        light_background: bool,
    ) -> Vec<Face> {
        let mut faces = Vec::new();
        let camera_distance = self.camera_distance;
        for (box_index, block) in boxes.iter().enumerate() {
            for axis in 0..3 {
                let u_axis = (axis + 1) % 3;
                let v_axis = (axis + 2) % 3;
                for side in [-1.0f64, 1.0] {
                    let plane = if side > 0.0 {
                        block.max[axis]
                    } else {
                        block.min[axis]
                    };
                    // Back faces point away from the camera.
                    if (origin[axis] - plane) * side <= 0.0 {
                        continue;
                    }
                    // Faces pressed against a neighboring block are hidden.
                    // Positions are exact while blocks rest, so the equality
                    // is exact in upstream too (`===`).
                    #[allow(clippy::float_cmp)]
                    let covered = boxes.iter().enumerate().any(|(other_index, other)| {
                        other_index != box_index
                            && (if side > 0.0 {
                                other.min[axis]
                            } else {
                                other.max[axis]
                            }) == plane
                            && other.min[u_axis] <= block.min[u_axis]
                            && other.max[u_axis] >= block.max[u_axis]
                            && other.min[v_axis] <= block.min[v_axis]
                            && other.max[v_axis] >= block.max[v_axis]
                    });
                    if covered {
                        continue;
                    }
                    let mut min_x = f64::INFINITY;
                    let mut min_y = f64::INFINITY;
                    let mut max_x = f64::NEG_INFINITY;
                    let mut max_y = f64::NEG_INFINITY;
                    let mut corner = [0.0f64; 3];
                    for corner_u in [block.min[u_axis], block.max[u_axis]] {
                        for corner_v in [block.min[v_axis], block.max[v_axis]] {
                            corner[axis] = plane;
                            corner[u_axis] = corner_u;
                            corner[v_axis] = corner_v;
                            let projected_x = matrix[0] * corner[0]
                                + matrix[1] * corner[1]
                                + matrix[2] * corner[2];
                            let projected_y = matrix[3] * corner[0]
                                + matrix[4] * corner[1]
                                + matrix[5] * corner[2];
                            let depth_z = matrix[6] * corner[0]
                                + matrix[7] * corner[1]
                                + matrix[8] * corner[2];
                            let perspective =
                                (pose.scale * camera_distance) / (camera_distance - depth_z);
                            let screen_x = pose.center_x + projected_x * perspective;
                            let screen_y = pose.center_y + projected_y * perspective;
                            min_x = min_x.min(screen_x);
                            min_y = min_y.min(screen_y);
                            max_x = max_x.max(screen_x);
                            max_y = max_y.max(screen_y);
                        }
                    }
                    let min_x = (min_x.floor().max(0.0)) as usize;
                    let min_y = (min_y.floor().max(0.0)) as usize;
                    let max_x = (max_x.ceil().min(dot_width as f64 - 1.0)) as usize;
                    let max_y = (max_y.ceil().min(dot_height as f64 - 1.0)) as usize;
                    if max_x < min_x || max_y < min_y {
                        continue;
                    }
                    // Faces are flat, so lighting is computed once per face.
                    // The normal in camera space is a column of the rotation
                    // matrix.
                    let nx = matrix[axis] * side;
                    let ny = matrix[3 + axis] * side;
                    let nz = matrix[6 + axis] * side;
                    let diffuse = (nx * LIGHT[0] + ny * LIGHT[1] + nz * LIGHT[2]).max(0.0);
                    let rim = (nx * 0.8 - nz * 0.3).max(0.0);
                    // On light backgrounds, highlights toward white would
                    // vanish, so faces only get darker than the brand colors
                    // there.
                    let specular = if light_background {
                        0.0
                    } else {
                        (nx * HALF_VECTOR[0] + ny * HALF_VECTOR[1] + nz * HALF_VECTOR[2])
                            .max(0.0)
                            .powf(24.0)
                            * 0.6
                            * 255.0
                    };
                    let light = if light_background {
                        (0.55 + diffuse * 0.45 + rim * 0.1).min(1.0)
                    } else {
                        0.45 + diffuse * 0.78 + rim * 0.25
                    };
                    faces.push(Face {
                        axis,
                        plane,
                        u_min: block.min[u_axis],
                        u_max: block.max[u_axis],
                        v_min: block.min[v_axis],
                        v_max: block.max[v_axis],
                        min_x,
                        min_y,
                        max_x,
                        max_y,
                        red: (block.color[0] * light + specular).min(255.0),
                        green: (block.color[1] * light + specular).min(255.0),
                        blue: (block.color[2] * light + specular).min(255.0),
                    });
                }
            }
        }
        faces
    }
}

/// A star of the background starfield (`prepareStars`'s entry).
struct Star {
    glyph: char,
    tint: [f64; 3],
    strength: f64,
    phase: f64,
    speed: f64,
}

/// The exit timeline snapshot (`close`'s `exit` object).
struct ExitState {
    start: Instant,
    /// The animation time when the exit began.
    time: f64,
    yaw: f64,
    target_yaw: f64,
    offsets: Vec<[f64; 3]>,
}

/// Everything the render pass mutates. One mutex because the [`Component`]
/// contract hands `render` only `&self`, and all mutation happens on the
/// interactive loop's thread.
struct Inner {
    raster: BlockRaster,
    exit: Option<ExitState>,
    /// The shuffle cache: `(cycle, steps)`.
    shuffle: Option<(u64, Vec<Vec<[i64; 3]>>)>,
    screen_width: usize,
    screen_height: usize,
    /// Star per cell index, or `None`.
    stars: Vec<Option<Star>>,
    cells: Vec<Vec<ScreenCell>>,
    ansi_cache: HashMap<i64, String>,
}

/// The 3D easter egg animation: the dissolve, the spin, the puzzle, and the
/// reassembly, drawn in braille over the captured screen
/// (`EasterEgg3dAnimation`).
pub struct EasterEgg3dAnimation {
    /// The screen to dissolve, as rendered lines.
    screen: Vec<String>,
    model: Model,
    foreground: [f64; 3],
    background: [f64; 3],
    /// The terminal's row count, refreshed by the host each pass (upstream
    /// reads `tui.terminal.rows` live).
    rows: Arc<AtomicU16>,
    /// Set once the exit animation completes; the host hides the overlay.
    done: Arc<AtomicBool>,
    start: Instant,
    inner: Mutex<Inner>,
}

/// What the host keeps to drive a playing easter egg.
pub struct EasterEggHandle {
    pub overlay: OverlayId,
    pub done: Arc<AtomicBool>,
    pub rows: Arc<AtomicU16>,
}

impl EasterEgg3dAnimation {
    fn new(
        screen: Vec<String>,
        model: Model,
        colors: ([f64; 3], [f64; 3]),
        rows: Arc<AtomicU16>,
        done: Arc<AtomicBool>,
    ) -> Self {
        let (foreground, background) = colors;
        Self {
            screen,
            model,
            foreground,
            background,
            rows,
            done,
            start: Instant::now(),
            inner: Mutex::new(Inner {
                raster: BlockRaster::new(0.0),
                exit: None,
                shuffle: None,
                screen_width: usize::MAX,
                screen_height: usize::MAX,
                stars: Vec::new(),
                cells: Vec::new(),
                ansi_cache: HashMap::new(),
            }),
        }
    }

    /// Play the exit animation. A second call skips it (`close`).
    fn close(&self) {
        let mut inner = self.inner.lock().expect("animation state");
        if inner.exit.is_some() {
            self.finish();
            return;
        }
        let time = self.elapsed();
        let yaw = spin_angle(time);
        let turn = std::f64::consts::TAU;
        let offsets = self.block_offsets(&mut inner.shuffle, time);
        inner.exit = Some(ExitState {
            start: Instant::now(),
            time,
            yaw,
            target_yaw: (yaw / turn).ceil() * turn,
            offsets,
        });
    }

    /// The exit completed: tell the host to hide the overlay (upstream's
    /// `finish` clears the interval and calls `onDone`).
    fn finish(&self) {
        self.done.store(true, Ordering::SeqCst);
    }

    fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn exit_progress(inner: &Inner) -> f64 {
        inner.exit.as_ref().map_or(0.0, |exit| {
            clamp01(exit.start.elapsed().as_secs_f64() / EXIT_DURATION)
        })
    }

    /// Each block's displacement from its place in the model at `time`, in
    /// grid units (`blockOffsets`). The per-cycle shuffle cache rides in
    /// `shuffle_cache`.
    fn block_offsets(
        &self,
        shuffle_cache: &mut Option<(u64, Vec<Vec<[i64; 3]>>)>,
        time: f64,
    ) -> Vec<[f64; 3]> {
        let blocks = &self.model.blocks;
        if time < PUZZLE_START {
            return vec![[0.0; 3]; blocks.len()];
        }
        let cycle = ((time - PUZZLE_START) / PUZZLE_CYCLE).floor() as u64;
        let local = time - PUZZLE_START - cycle as f64 * PUZZLE_CYCLE;
        if shuffle_cache
            .as_ref()
            .is_none_or(|(cached, _)| *cached != cycle)
        {
            *shuffle_cache = Some((cycle, shuffle_steps(&self.model, cycle)));
        }
        let steps = &shuffle_cache.as_ref().expect("just cached").1;
        let offset = |position: [i64; 3], index: usize| -> [f64; 3] {
            let home = blocks[index].home;
            [
                position[0] as f64 - home[0] as f64,
                position[1] as f64 - home[1] as f64,
                position[2] as f64 - home[2] as f64,
            ]
        };
        let shuffle_end = PUZZLE_STEP * PUZZLE_STEPS as f64;
        if local < shuffle_end {
            let step = (local / PUZZLE_STEP).floor() as usize;
            let progress = smooth((local - step as f64 * PUZZLE_STEP) / PUZZLE_STEP);
            return steps[step]
                .iter()
                .zip(steps[step + 1].iter())
                .enumerate()
                .map(|(index, (from, to))| {
                    let a = offset(*from, index);
                    let b = offset(*to, index);
                    [
                        a[0] + (b[0] - a[0]) * progress,
                        a[1] + (b[1] - a[1]) * progress,
                        a[2] + (b[2] - a[2]) * progress,
                    ]
                })
                .collect();
        }
        if local < shuffle_end + PUZZLE_RETURN {
            // All blocks fly home at once. Alternating arcs in depth keep
            // them from passing through each other.
            let u = (local - shuffle_end) / PUZZLE_RETURN;
            let remaining = 1.0 - smooth(u);
            let arc = (std::f64::consts::PI * clamp01(u)).sin() * 0.8;
            return steps[PUZZLE_STEPS as usize]
                .iter()
                .enumerate()
                .map(|(index, position)| {
                    let [x, y, z] = offset(*position, index);
                    [
                        x * remaining,
                        y * remaining,
                        z * remaining + if index % 2 == 0 { arc } else { -arc },
                    ]
                })
                .collect();
        }
        vec![[0.0; 3]; blocks.len()]
    }

    fn fly_progress(time: f64) -> f64 {
        smooth((time - FLY_START) / FLY_DURATION)
    }

    fn star_alpha(time: f64) -> f64 {
        smooth((time - STARS_START) / STARS_FADE)
    }

    fn hint_alpha(time: f64) -> f64 {
        smooth((time - FLY_START - FLY_DURATION) / HINT_FADE)
    }

    fn pose(&self, width: usize, height: usize, progress: f64, time: f64) -> Pose {
        let model = &self.model;
        let reach = model.radius * (model.camera_distance / (model.camera_distance - model.radius));
        // Lifting off, the front face must cover exactly the half-block
        // cells (2x2 dots per pixel) despite the perspective.
        let start_scale = if model.origin.is_some() {
            2.0 * (model.camera_distance - DEPTH / 2.0) / model.camera_distance
        } else {
            START_SCALE
        };
        let end_scale = (start_scale).max(
            (width as f64 * 2.0 * model.width_share).min((height as f64 * 4.0 - 8.0) * 0.48)
                / reach,
        );
        let end_x = width as f64;
        let end_y = height as f64 * 2.0 - 2.0;
        let (start_x, start_y) = match model.origin {
            Some((column, row)) => (
                column as f64 * 2.0 + model.columns as f64,
                row as f64 * 4.0 + model.rows as f64,
            ),
            None => (end_x, end_y),
        };
        Pose {
            center_x: start_x + (end_x - start_x) * progress,
            center_y: start_y + (end_y - start_y) * progress,
            // Interpolate the zoom geometrically so it feels uniform.
            scale: start_scale * (end_scale / start_scale).powf(progress),
            yaw: spin_angle(time),
            // Tilts are zero at whole turns, so the camera looks straight at
            // the front of the reassembled logo.
            pitch: 0.3 * spin_phase(time).sin() * progress,
            roll: 0.06 * (2.0 * spin_phase(time)).sin() * progress,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn hint(width: usize, height: usize, alpha: f64, background: [f64; 3]) -> Option<Hint> {
        if alpha <= 0.0 || height < 2 {
            return None;
        }
        let key = super::super::interactive_mode::key_display_text(action::SELECT_CANCEL);
        let text = format!("{key} to return");
        if text.len() > width {
            return None;
        }
        let tint = |token: ThemeColor| -> [f64; 3] {
            let rgb = color_to_rgb(theme::theme().fg_color(token));
            [rgb.r, rgb.g, rgb.b]
        };
        Some(Hint {
            row: height - 2,
            start: (width - text.len()) / 2,
            text,
            key_length: key.len(),
            key_color: mix(background, tint(ThemeColor::Muted), alpha),
            color: mix(background, tint(ThemeColor::Dim), alpha),
        })
    }

    /// A sparse, deterministic starfield: one braille dot in about
    /// `STAR_DENSITY` of all cells (`prepareStars`).
    fn prepare_stars(width: usize, height: usize) -> Vec<Option<Star>> {
        (0..width * height)
            .map(|index| {
                if hash((index as u64) * 3 + 0x51ed) >= STAR_DENSITY {
                    return None;
                }
                let random = |salt: u64| hash((index as u64) * 7 + salt * 0x9e37);
                Some(Star {
                    glyph: braille(DOT_BITS[(random(1) * 8.0) as usize]),
                    tint: STAR_TINTS[(random(2) * STAR_TINTS.len() as f64) as usize],
                    strength: 0.1 + random(3) * 0.12,
                    phase: random(4) * std::f64::consts::TAU,
                    speed: 0.4 + random(5) * 0.8,
                })
            })
            .collect()
    }

    fn prepare_cells(
        &self,
        width: usize,
        height: usize,
        foreground: [f64; 3],
    ) -> Vec<Vec<ScreenCell>> {
        let mut cells = parse_screen(&self.screen, width, foreground, self.background);
        let model = &self.model;
        // The dust spreads out from where the model starts.
        let (center_x, center_y) = match model.origin {
            Some((column, row)) => (
                column as f64 + model.columns as f64 / 2.0,
                row as f64 + model.rows as f64 / 4.0,
            ),
            None => (width as f64 / 2.0, height as f64 / 2.0),
        };
        let farthest = (width as f64 - center_x)
            .max(center_x)
            .hypot((height as f64 - center_y).max(center_y) * 2.0);
        for (row, line) in cells.iter_mut().enumerate() {
            for column in 0..line.len() {
                // Wide graphemes share their first column's timing so both
                // halves change together.
                let owner_text = if line[column].width == 0 {
                    line[..column]
                        .iter()
                        .next_back()
                        .map(|owner| owner.text.clone())
                        .unwrap_or_default()
                } else {
                    line[column].text.clone()
                };
                let cell = &mut line[column];
                let seed = hash((row as u64) * 65_537 + column as u64);
                if cell.width != 0 {
                    let distance = ((column as f64 - center_x)
                        .hypot((row as f64 - center_y) * 2.0))
                        / farthest;
                    cell.delay = FLY_START + distance * WAVE_SPREAD + seed * WAVE_JITTER;
                }
                cell.seed = (row as u64) * 65_537 + column as u64;
                cell.ink = glyph_ink(&owner_text, seed);
            }
        }
        // The 3D model replaces its half-block rendering on screen.
        if let Some((origin_column, origin_row)) = model.origin {
            for row in origin_row..origin_row + model.rows.div_ceil(2) {
                let Some(line) = cells.get_mut(row) else {
                    continue;
                };
                for column in origin_column..origin_column + model.columns {
                    if let Some(cell) = line.get_mut(column) {
                        cell.text = " ".to_string();
                        cell.width = 1;
                        cell.ink = 0;
                        cell.bg = None;
                    }
                }
            }
        }
        cells
    }
}

/// The hint line rendered near the bottom of the screen.
struct Hint {
    row: usize,
    start: usize,
    text: String,
    key_length: usize,
    key_color: [f64; 3],
    color: [f64; 3],
}

fn braille(bits: u8) -> char {
    char::from_u32(0x2800 + u32::from(bits)).expect("braille range")
}

impl Component for EasterEgg3dAnimation {
    #[allow(clippy::too_many_lines)] // mirrors the TS animation frame; split when extending
    fn render(&self, width: usize) -> Vec<String> {
        let height = usize::from(self.rows.load(Ordering::SeqCst)).max(1);
        let foreground = self.foreground;
        let background = self.background;
        let mut inner = self.inner.lock().expect("animation state");
        // Upstream's timer stops the animation once the exit completes; the
        // port tells the host here, at the same point in the timeline.
        let exit_started = inner.exit.is_some();
        let progress = if exit_started {
            Self::exit_progress(&inner)
        } else {
            0.0
        };
        if exit_started && progress >= 1.0 {
            self.finish();
        }
        // Split the guard into disjoint field borrows: the row loop below
        // mutates the ANSI cache while reading the raster and cells.
        let Inner {
            raster,
            exit,
            shuffle,
            screen_width,
            screen_height,
            stars,
            cells,
            ansi_cache,
        } = &mut *inner;
        if width != *screen_width || height != *screen_height {
            *screen_width = width;
            *screen_height = height;
            *cells = self.prepare_cells(width, height, foreground);
            *stars = Self::prepare_stars(width, height);
        }
        // Dissolve time runs forward on entry and backward on exit, so the
        // screen reassembles in reverse order.
        let (dissolve_time, pose, hint_alpha, star_alpha, offsets);
        if let Some(exit) = exit.as_ref() {
            let landing = clamp01(progress / 0.8);
            let settle = 1.0 - smooth(landing);
            dissolve_time = exit.time.min(DISSOLVE_END) * (1.0 - progress);
            let mut exit_pose = self.pose(
                width,
                height,
                Self::fly_progress(exit.time) * settle,
                exit.time,
            );
            exit_pose.yaw = exit.yaw + (exit.target_yaw - exit.yaw) * ease_out(landing);
            exit_pose.pitch *= settle;
            exit_pose.roll *= settle;
            pose = exit_pose;
            hint_alpha = Self::hint_alpha(exit.time) * (1.0 - smooth(progress / 0.2));
            star_alpha = Self::star_alpha(exit.time) * (1.0 - smooth(progress / 0.3));
            // Blocks are home well before the logo lands.
            let gather = 1.0 - smooth(progress / 0.5);
            offsets = exit
                .offsets
                .iter()
                .map(|offset| [offset[0] * gather, offset[1] * gather, offset[2] * gather])
                .collect();
        } else {
            let time = self.elapsed();
            dissolve_time = time;
            pose = self.pose(width, height, Self::fly_progress(time), time);
            hint_alpha = Self::hint_alpha(time);
            star_alpha = Self::star_alpha(time);
            offsets = self.block_offsets(shuffle, time);
        }
        let model = &self.model;
        let boxes: Vec<Box3> = model
            .blocks
            .iter()
            .zip(offsets.iter())
            .map(|(block, offset)| {
                let x = block.home[0] as f64 - model.columns as f64 / 2.0 + offset[0];
                let y = block.home[1] as f64 - model.rows as f64 / 2.0 + offset[1];
                Box3 {
                    min: [x, y, offset[2] - DEPTH / 2.0],
                    max: [x + 1.0, y + 1.0, offset[2] + DEPTH / 2.0],
                    color: block.color,
                }
            })
            .collect();
        raster.camera_distance = model.camera_distance;
        raster.render(
            width,
            height,
            &pose,
            &boxes,
            background,
            if is_light(background) {
                LIGHT_HALO_STRENGTH
            } else {
                0.0
            },
        );
        let background_fade = smooth(dissolve_time / BACKGROUND_FADE);
        let text_fade = smooth(dissolve_time / 0.6) * 0.3;
        // Once the screen has fully dissolved, the text layer is empty and
        // can be skipped.
        let text_active = dissolve_time < DISSOLVE_END || background_fade < 1.0;
        let hint = Self::hint(width, height, hint_alpha, background);
        let frame = (dissolve_time * 14.0) as u64;
        let now = self.elapsed();
        // Star brightness is quantized, so a row only changes when one of
        // its stars steps to another level instead of on every frame.
        let star_level = (star_alpha * STAR_ALPHA_LEVELS).round() / STAR_ALPHA_LEVELS;
        let mut lines: Vec<String> = Vec::with_capacity(height);
        for row in 0..height {
            let mut emitter = Emitter {
                line: String::new(),
                current_fg: -1,
                current_bg: -1,
                cache: &mut *ansi_cache,
            };
            let mut column = 0;
            while column < width {
                let index = row * width + column;
                let cell = if text_active {
                    cells.get(row).and_then(|line| line.get(column))
                } else {
                    None
                };
                let cell_bg = halo_tint(
                    raster,
                    index,
                    match cell {
                        Some(cell) => cell
                            .bg
                            .filter(|_| background_fade < 1.0)
                            .map(|bg| mix(bg, background, background_fade)),
                        None => None,
                    },
                    background,
                );
                if let Some(hint) = &hint {
                    if row == hint.row
                        && column >= hint.start
                        && column < hint.start + hint.text.len()
                    {
                        let offset = column - hint.start;
                        let color = if offset < hint.key_length {
                            hint.key_color
                        } else {
                            hint.color
                        };
                        let char = hint.text.chars().nth(offset).expect("in range");
                        emitter.emit(&char.to_string(), Some(color), cell_bg);
                        column += 1;
                        continue;
                    }
                }
                let dots = raster.bits[index];
                if dots != 0 {
                    // Quantized so neighboring cells on one face usually
                    // share a color and its escape sequence.
                    let step = LOGO_COLOR_STEP * f64::from(raster.counts[index]);
                    let rgb = &raster.rgb;
                    let color = [
                        (f64::from(rgb[index * 3]) / step).round() * LOGO_COLOR_STEP,
                        (f64::from(rgb[index * 3 + 1]) / step).round() * LOGO_COLOR_STEP,
                        (f64::from(rgb[index * 3 + 2]) / step).round() * LOGO_COLOR_STEP,
                    ];
                    emitter.emit(&braille(dots).to_string(), Some(color), cell_bg);
                    column += 1;
                    continue;
                }
                let Some(cell) = cell else {
                    // Beyond the text layer: starfield only.
                    let star = starfield(stars, index, star_level, now, foreground, background);
                    let (text, fg) = match star {
                        Some(star) => (star.0, Some(star.1)),
                        None => (" ".to_string(), None),
                    };
                    emitter.emit(&text, fg, cell_bg);
                    column += 1;
                    continue;
                };
                let dust = (dissolve_time - cell.delay) / DUST_DURATION;
                if dust < 0.0 && cell.width == 2 && raster.bits.get(index + 1) == Some(&0) {
                    emitter.emit(
                        &cell.text,
                        Some(mix(cell.fg, background, text_fade)),
                        cell_bg,
                    );
                    // Skip the wide glyph's continuation cell.
                    column += 2;
                    continue;
                }
                if dust < 0.0 && cell.width == 1 {
                    emitter.emit(
                        &cell.text,
                        Some(mix(cell.fg, background, text_fade)),
                        cell_bg,
                    );
                    column += 1;
                    continue;
                }
                let dot_count = if dust < 0.0 {
                    cell.ink
                } else {
                    (f64::from(cell.ink) * (1.0 - clamp01(dust))).round() as u32
                };
                if dot_count == 0 {
                    let star = starfield(stars, index, star_level, now, foreground, background);
                    let (text, fg) = match star {
                        Some(star) => (star.0, Some(star.1)),
                        None => (" ".to_string(), None),
                    };
                    emitter.emit(&text, fg, cell_bg);
                    column += 1;
                    continue;
                }
                // Pick `dotCount` distinct dots; the choice changes a few
                // times per second so the dust shimmers.
                let mut bits = 0u8;
                let mut placed = 0u32;
                for attempt in 0u64..32 {
                    if placed >= dot_count {
                        break;
                    }
                    let bit_index = hash(cell.seed.wrapping_mul(7_919) + frame * 131 + attempt);
                    let bit = DOT_BITS[(bit_index * 8.0) as usize];
                    if bits & bit != 0 {
                        continue;
                    }
                    bits |= bit;
                    placed += 1;
                }
                let fade = text_fade + (1.0 - text_fade) * clamp01(dust).powf(0.8);
                emitter.emit(
                    &braille(bits).to_string(),
                    Some(mix(cell.fg, background, fade)),
                    cell_bg,
                );
                column += 1;
            }
            lines.push(format!("{}\x1b[0m", emitter.line));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<pi_tui::component::ComponentMutation> {
        // `tui.select.cancel` and `app.clear` both close
        // (`easter-egg-3d.ts:935-937`).
        if keybindings::matches(data, action::SELECT_CANCEL)
            || theme::app_keys::matches(data, theme::app_keys::CLEAR)
        {
            self.close();
        }
        None
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if event.kind == MouseEventKind::Click {
            self.close();
        }
        Some(MouseEventResult::handled_with_render(false))
    }

    fn invalidate(&mut self) {
        let mut inner = self.inner.lock().expect("animation state");
        inner.screen_width = usize::MAX;
        inner.screen_height = usize::MAX;
        inner.ansi_cache.clear();
    }
}

type StarDraw = (String, [f64; 3]);

fn starfield(
    stars: &[Option<Star>],
    index: usize,
    star_level: f64,
    now: f64,
    foreground: [f64; 3],
    background: [f64; 3],
) -> Option<StarDraw> {
    if star_level <= 0.0 {
        return None;
    }
    let star = stars.get(index).and_then(|entry| entry.as_ref())?;
    let flicker = ((0.5 + 0.5 * (now * star.speed + star.phase).sin()) * STAR_FLICKER_LEVELS)
        .round()
        / STAR_FLICKER_LEVELS;
    Some((
        star.glyph.to_string(),
        mix(
            background,
            mix(foreground, star.tint, 0.5),
            star.strength * (0.7 + 0.3 * flicker) * star_level,
        ),
    ))
}

/// The cell's halo-tinted background, if any (`render`'s `halo` closure).
fn halo_tint(
    raster: &BlockRaster,
    index: usize,
    base: Option<[f64; 3]>,
    background: [f64; 3],
) -> Option<[f64; 3]> {
    let amount = raster.halo_amount[index];
    if amount <= 0.0 {
        return base;
    }
    let color = &raster.halo_rgb;
    Some(mix(
        base.unwrap_or(background),
        [
            f64::from(color[index * 3]),
            f64::from(color[index * 3 + 1]),
            f64::from(color[index * 3 + 2]),
        ],
        f64::from(amount),
    ))
}

/// The per-row escape-sequence state machine (`render`'s `emit` closure).
struct Emitter<'a> {
    line: String,
    current_fg: i64,
    current_bg: i64,
    cache: &'a mut HashMap<i64, String>,
}

impl Emitter<'_> {
    fn emit(&mut self, text: &str, fg: Option<[f64; 3]>, bg: Option<[f64; 3]>) {
        let fg_key = fg.map_or(-1, Self::key);
        let bg_key = bg.map_or(-1, Self::key);
        if fg_key != self.current_fg && text != " " {
            let sequence = match fg {
                Some(_) => self.ansi(fg_key, false),
                None => "\x1b[39m".to_string(),
            };
            self.line.push_str(&sequence);
            self.current_fg = fg_key;
        }
        if bg_key != self.current_bg {
            let sequence = match bg {
                Some(_) => self.ansi(bg_key, true),
                None => "\x1b[49m".to_string(),
            };
            self.line.push_str(&sequence);
            self.current_bg = bg_key;
        }
        self.line.push_str(text);
    }

    fn key(color: [f64; 3]) -> i64 {
        ((color[0].round() as i64) << 16)
            | ((color[1].round() as i64) << 8)
            | color[2].round() as i64
    }

    fn ansi(&mut self, key: i64, is_background: bool) -> String {
        let cache_key = key * 2 + i64::from(is_background);
        if let Some(value) = self.cache.get(&cache_key) {
            return value.clone();
        }
        let color = rgb_color(
            ((key >> 16) & 255) as f64,
            ((key >> 8) & 255) as f64,
            (key & 255) as f64,
        );
        let mode = theme::theme().color_mode();
        let value = if is_background {
            background_ansi(color, mode)
        } else {
            foreground_ansi(color, mode)
        };
        self.cache.insert(cache_key, value.clone());
        value
    }
}

/// Show the animation as a fullscreen overlay until it is dismissed. The
/// overlay takes focus and mouse input and returns focus when hidden, so the
/// rest of the UI keeps running underneath untouched (`playEasterEgg3d`).
///
/// The caller has already checked the fullscreen guard (upstream's
/// `tui instanceof TuiAltScreen`) and captured the screen.
pub fn play_easter_egg_3d(tui: &mut Tui, screen: Vec<String>, egg: EasterEgg3d) -> EasterEggHandle {
    // Fading needs the terminal's actual default colors; the theme only
    // knows its own. The startup query reported them; fall back like
    // upstream when it did not.
    let reported = theme::terminal_colors();
    let dark = theme::terminal_appearance() == ThemeAppearance::Dark;
    let tint = |token: ThemeColor| -> [f64; 3] {
        let rgb = color_to_rgb(theme::theme().fg_color(token));
        [rgb.r, rgb.g, rgb.b]
    };
    let foreground = reported
        .foreground
        .map_or_else(|| tint(ThemeColor::Text), |rgb| [rgb.r, rgb.g, rgb.b]);
    let background = reported.background.map_or_else(
        || if dark { [0.0, 0.0, 0.0] } else { [255.0; 3] },
        |rgb| [rgb.r, rgb.g, rgb.b],
    );
    let model = match egg {
        EasterEgg3d::Armin => armin_model(tint(ThemeColor::Accent)),
        EasterEgg3d::PiLogo { column, row } => pi_logo_model(Some((column, row))),
    };
    let rows = Arc::new(AtomicU16::new(
        tui.terminal()
            .size()
            .map(|size| size.rows)
            .unwrap_or_default(),
    ));
    let done = Arc::new(AtomicBool::new(false));
    let animation = EasterEgg3dAnimation::new(
        screen,
        model,
        (foreground, background),
        Arc::clone(&rows),
        Arc::clone(&done),
    );
    let overlay = tui.show_overlay(
        Box::new(animation),
        Some(OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Percent(100.0)),
            max_height: Some(SizeValue::Percent(100.0)),
            ..OverlayOptions::default()
        }),
    );
    EasterEggHandle {
        overlay,
        done,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logo_model() -> Model {
        pi_logo_model(Some((0, 0)))
    }

    #[test]
    fn the_models_match_their_bitmaps() {
        let model = logo_model();
        assert_eq!(model.columns, 4);
        assert_eq!(model.rows, 4);
        // "ccc.", "b.c.", "bb.y", "b..y" — 10 blocks in row-major order:
        // coral coral coral, blue coral, blue blue yellow, blue yellow.
        assert_eq!(model.blocks.len(), 10);
        assert_eq!(model.blocks[0].home, [0, 0, 0]);
        assert_eq!(model.blocks[2].color, CORAL, "row 0 is all coral");
        assert_eq!(model.blocks[3].color, BLUE, "row 1 starts blue");
        assert_eq!(model.blocks[4].color, CORAL, "row 1 ends coral");
        assert_eq!(model.blocks[7].color, YELLOW, "row 2 ends yellow");
        assert_eq!(model.blocks[9].color, YELLOW, "row 3 ends yellow");
        let armin = armin_model([1.0, 2.0, 3.0]);
        assert_eq!(armin.blocks.len(), 245, "one block per art pixel");
        assert_eq!(armin.camera_distance, 80.0);
        assert!(armin.origin.is_none());
    }

    #[test]
    fn hash_is_deterministic_and_in_range() {
        for value in 0..1000u64 {
            let hashed = hash(value);
            assert!((0.0..1.0).contains(&hashed), "hash({value}) = {hashed}");
            assert_eq!(hashed, hash(value), "deterministic");
        }
        // Distinct inputs spread out; spot-check two against the JS values
        // implied by the bit mixer.
        assert_ne!(hash(1), hash(2));
    }

    #[test]
    fn rotation_is_orthonormal() {
        let m = rotation(0.7, -0.3, 1.1);
        for row in 0..3 {
            let length = (m[row * 3] * m[row * 3]
                + m[row * 3 + 1] * m[row * 3 + 1]
                + m[row * 3 + 2] * m[row * 3 + 2])
                .sqrt();
            assert!((length - 1.0).abs() < 1e-12, "row {row} length {length}");
        }
        for (a, b) in [(0, 1), (1, 2), (0, 2)] {
            let dot =
                m[a * 3] * m[b * 3] + m[a * 3 + 1] * m[b * 3 + 1] + m[a * 3 + 2] * m[b * 3 + 2];
            assert!(dot.abs() < 1e-12, "rows {a}/{b} dot {dot}");
        }
        // Identity at zero angles.
        let identity = rotation(0.0, 0.0, 0.0);
        assert_eq!(identity, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn shuffle_steps_stay_in_bounds_and_unique() {
        let model = logo_model();
        let steps = shuffle_steps(&model, 3);
        assert_eq!(steps.len(), PUZZLE_STEPS as usize + 1);
        // Step 0 is home.
        for (position, block) in steps[0].iter().zip(&model.blocks) {
            assert_eq!(*position, block.home);
        }
        for (step, positions) in steps.iter().enumerate() {
            let unique: std::collections::HashSet<[i64; 3]> = positions.iter().copied().collect();
            assert_eq!(unique.len(), positions.len(), "step {step} overlaps");
            for position in positions {
                assert!(
                    position[0] >= 0
                        && position[0] < model.columns as i64
                        && position[1] >= 0
                        && position[1] < model.rows as i64
                        && (-1..=1).contains(&position[2]),
                    "step {step} position {position:?} out of bounds"
                );
            }
        }
        // Deterministic per cycle.
        assert_eq!(shuffle_steps(&model, 3), steps);
        assert_ne!(shuffle_steps(&model, 4), steps, "cycles differ");
    }

    #[test]
    fn parse_screen_resolves_sgr_state() {
        let foreground = [10.0, 20.0, 30.0];
        let background = [200.0, 200.0, 200.0];
        let lines = vec![
            "plain".to_string(),
            "\x1b[31mred \x1b[44mon blue".to_string(),
            "\x1b[7minverse\x1b[27m back".to_string(),
            "\x1b[38;5;196me\x1b[0m".to_string(),
            "\x1b[2mdim\x1b[22m".to_string(),
        ];
        let cells = parse_screen(&lines, 40, foreground, background);
        assert_eq!(cells.len(), 5);
        let plain = &cells[0];
        assert_eq!(plain.len(), 5);
        assert_eq!(plain[0].text, "p");
        assert_eq!(plain[0].fg, foreground);
        assert!(plain[0].bg.is_none());
        // 31 → palette(1), then 44 → palette(4) behind it.
        let red = &cells[1][0];
        let Rgb { r, g, b } = color_to_rgb(indexed_color(1));
        assert_eq!(red.fg, [r, g, b]);
        let on_blue = &cells[1][4];
        let Rgb { r, g, b } = color_to_rgb(indexed_color(4));
        assert_eq!(on_blue.bg, Some([r, g, b]));
        // Inverse swaps fg and bg.
        let inverse = &cells[2][0];
        assert_eq!(inverse.fg, background);
        assert_eq!(inverse.bg, Some(foreground));
        // 256-color extended foreground.
        let extended = &cells[3][0];
        let Rgb { r, g, b } = color_to_rgb(indexed_color(196));
        assert_eq!(extended.fg, [r, g, b]);
        // Dim mixes the foreground toward the background.
        let dim = &cells[4][0];
        assert_eq!(dim.fg, mix(foreground, background, 0.4));
    }

    #[test]
    fn parse_screen_clamps_to_the_width_and_marks_wide_cells() {
        let cells = parse_screen(&["ab".to_string()], 3, [0.0; 3], [0.0; 3]);
        assert_eq!(cells[0].len(), 2);
        // A wide glyph occupies two cells, the second a continuation.
        let wide = parse_screen(&["漢".to_string()], 10, [0.0; 3], [0.0; 3]);
        assert_eq!(wide[0].len(), 2);
        assert_eq!(wide[0][0].width, 2);
        assert_eq!(wide[0][1].width, 0);
        assert_eq!(wide[0][1].text, "");
    }

    #[test]
    fn the_raster_draws_lit_cells_and_survives_resize() {
        let mut raster = BlockRaster::new(10.0);
        let pose = Pose {
            center_x: 40.0,
            center_y: 40.0,
            scale: 10.0,
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
        };
        let blocks = [Box3 {
            min: [-1.0, -1.0, -DEPTH / 2.0],
            max: [1.0, 1.0, DEPTH / 2.0],
            color: [200.0, 100.0, 50.0],
        }];
        raster.render(40, 20, &pose, &blocks, [0.0; 3], 0.0);
        let lit: usize = raster.bits.iter().filter(|bits| **bits != 0).count();
        assert!(lit > 0, "a face-on block lights cells");
        // Same size: buffers reused, still lit.
        raster.render(40, 20, &pose, &blocks, [0.0; 3], 0.0);
        let lit_again = raster.bits.iter().filter(|bits| **bits != 0).count();
        assert_eq!(lit, lit_again, "buffers reset cleanly between frames");
        // Resize reallocates without panicking.
        raster.render(20, 10, &pose, &blocks, [0.0; 3], 0.0);
        assert_eq!(raster.width, 20);
    }

    #[test]
    fn the_spin_lingers_at_front_views() {
        // At the first assembled front view the phase is a whole number of
        // turns, and the yaw lingers exactly there.
        let angle = spin_angle(FIRST_FRONT_VIEW);
        let turns = angle / std::f64::consts::TAU;
        assert!(
            (turns - turns.round()).abs() < 1e-9,
            "front view at whole turns, got {angle}"
        );
        // Zero time faces front too.
        assert!(spin_angle(0.0).abs() < 1e-9);
    }

    fn test_animation(origin: Option<(usize, usize)>) -> (EasterEgg3dAnimation, Arc<AtomicBool>) {
        let screen: Vec<String> = vec!["hello world".to_string(), "second line".to_string()];
        let done = Arc::new(AtomicBool::new(false));
        let rows = Arc::new(AtomicU16::new(10));
        (
            EasterEgg3dAnimation::new(
                screen,
                pi_logo_model(origin),
                ([255.0; 3], [0.0; 3]),
                rows,
                Arc::clone(&done),
            ),
            done,
        )
    }

    #[test]
    fn the_animation_renders_the_full_screen() {
        let (animation, _done) = test_animation(Some((0, 0)));
        let lines = animation.render(40);
        assert_eq!(lines.len(), 10, "one line per terminal row");
        for line in &lines {
            assert!(line.ends_with("\x1b[0m"), "each row resets its style");
            assert!(pi_tui::ansi::visible_width(line) <= 40);
        }
        // The text layer dissolves: at t=0 the cells are still (mostly)
        // visible, so the art rows show the screen's words around the model.
        let plain = pi_tui::ansi::strip_terminal_sequences(&lines[0]);
        // The logo lifts off over its own origin cells, so "hello" is
        // partially under it while the rest of the line is still text.
        assert!(plain.contains("world"), "t=0 keeps the text: {plain:?}");
    }

    #[test]
    fn close_plays_the_exit_and_reports_done() {
        let (animation, done) = test_animation(None);
        assert!(!done.load(Ordering::SeqCst));
        animation.close();
        {
            let inner = animation.inner.lock().expect("animation state");
            assert!(inner.exit.is_some(), "close arms the exit timeline");
        }
        // A second close is a no-op skip.
        animation.close();
        // Backdate the exit past EXIT_DURATION: the next render reports done.
        {
            let mut inner = animation.inner.lock().expect("animation state");
            let exit = inner.exit.as_mut().expect("exit armed");
            exit.start = Instant::now()
                .checked_sub(std::time::Duration::from_millis(
                    (EXIT_DURATION * 1000.0 + 100.0) as u64,
                ))
                .expect("backdate");
        }
        let lines = animation.render(40);
        assert_eq!(lines.len(), 10);
        assert!(
            done.load(Ordering::SeqCst),
            "the completed exit tells the host to hide the overlay"
        );
    }

    /// The exit runs the intro backwards (`render`'s exit branch,
    /// `easter-egg-3d.ts:962-981`): from the fully dissolved screen the text
    /// layer reassembles, and the completed exit finishes the animation
    /// (`exitProgress`, `easter-egg-3d.ts:1114-1118`, finishing at >= 1).
    #[test]
    fn the_exit_reassembles_the_screen() {
        let (animation, done) = test_animation(Some((0, 0)));
        // Age the intro past the dissolve (`DISSOLVE_END`) so the exit starts
        // from a fully dissolved screen.
        animation.start = Instant::now()
            .checked_sub(std::time::Duration::from_millis(3000))
            .expect("backdate");
        animation.close();
        let lines = animation.render(40);
        let plain = pi_tui::ansi::strip_terminal_sequences(&lines[0]);
        assert!(
            !plain.contains("world"),
            "the exit starts from the dissolved screen: {plain:?}"
        );
        // Run the exit to completion: the same render pass reports done
        // (upstream's interval finishes when `exitProgress() >= 1`) and the
        // captured screen is back, reassembled in reverse order.
        {
            let mut inner = animation.inner.lock().expect("animation state");
            let exit = inner.exit.as_mut().expect("exit armed");
            exit.start = Instant::now()
                .checked_sub(std::time::Duration::from_millis(
                    (EXIT_DURATION * 1000.0 + 100.0) as u64,
                ))
                .expect("backdate");
        }
        let lines = animation.render(40);
        assert!(done.load(Ordering::SeqCst), "the completed exit finishes");
        let plain = pi_tui::ansi::strip_terminal_sequences(&lines[0]);
        assert!(plain.contains("world"), "the screen reassembles: {plain:?}");
    }

    /// `handleInput` closes on cancel and clear (`easter-egg-3d.ts:935-937`):
    /// escape and ctrl+c arm the exit, and a second close skips straight to
    /// finish.
    #[test]
    fn cancel_and_clear_input_close_the_egg() {
        let (mut animation, done) = test_animation(None);
        let _ = animation.handle_input("\x1b");
        {
            let inner = animation.inner.lock().expect("animation state");
            assert!(inner.exit.is_some(), "escape arms the exit");
        }
        assert!(!done.load(Ordering::SeqCst), "one close only arms the exit");
        // ctrl+c matches `tui.select.cancel` and `app.clear` alike; with the
        // exit already armed this second close finishes immediately.
        let _ = animation.handle_input("\x03");
        assert!(
            done.load(Ordering::SeqCst),
            "a second close skips to finish"
        );
    }

    #[test]
    fn glyph_ink_measures_ink_by_class() {
        assert_eq!(glyph_ink(".", 0.0), 2);
        assert_eq!(glyph_ink(",", 0.0), 2);
        assert_eq!(glyph_ink("─", 0.0), 3, "box drawing");
        assert_eq!(glyph_ink("A", 0.0), 4);
        assert_eq!(glyph_ink("A", 0.9), 6, "4 + floor(seed * 3)");
        assert_eq!(glyph_ink("  ", 0.0), 0);
    }

    #[test]
    fn prepare_cells_clears_the_model_origin() {
        let (animation, _done) = test_animation(Some((0, 0)));
        let cells = animation.prepare_cells(40, 10, [255.0; 3]);
        // The 4x4-pixel logo at (0, 0) replaces its 2-row, 4-column
        // half-block rendering with blanks.
        for (row, cells_row) in cells.iter().enumerate().take(2) {
            for (column, cell) in cells_row.iter().enumerate().take(4) {
                assert_eq!(cell.text, " ", "({column}, {row}) cleared");
                assert_eq!(cell.ink, 0);
                assert!(cell.bg.is_none());
            }
        }
        assert_eq!(cells[0][5].text, " ", "outside stays blank too");
        assert!(cells[0][5].delay > 0.0, "dust delays are assigned");
    }
}

//! Armin says hi! A fun easter egg with animated XBM art.
//!
//! Port of `components/armin.js`. The upstream component owns a
//! `setInterval` and advances an effect state per tick; this port has no
//! timers — the interactive loop renders every pass, so [`ArminComponent`]
//! derives its frame from elapsed time and reproduces each effect as a pure
//! function of `(effect, seed, frame)`. Upstream's `Math.random` calls become
//! a seeded split-mix generator, so a frame is reproducible while the
//! per-run effect choice and layout stay random like upstream's.
//!
//! The final image stays in the transcript after the animation ends, exactly
//! like upstream leaving the component in the chat with its interval stopped.
//! The rain's re-randomized settle bounce is fixed per column here (the
//! replay-to-frame evaluation cannot re-roll it), which only shifts which
//! pixels rain together, not the effect.

use std::time::Instant;

use pi_tui::component::Component;
use pi_tui::theme::ThemeColor;

use super::super::theme;

/// XBM image: 31x36 pixels, LSB first, 1=background, 0=foreground.
pub const ARMIN_WIDTH: usize = 31;
pub const ARMIN_HEIGHT: usize = 36;

const BITS: [u8; 144] = [
    0xff, 0xff, 0xff, 0x7f, 0xff, 0xf0, 0xff, 0x7f, 0xff, 0xed, 0xff, 0x7f, //
    0xff, 0xdb, 0xff, 0x7f, 0xff, 0xb7, 0xff, 0x7f, 0xff, 0x77, 0xfe, 0x7f, //
    0x3f, 0xf8, 0xfe, 0x7f, 0xdf, 0xff, 0xfe, 0x7f, 0xdf, 0x3f, 0xfc, 0x7f, //
    0x9f, 0xc3, 0xfb, 0x7f, 0x6f, 0xfc, 0xf4, 0x7f, 0xf7, 0x0f, 0xf7, 0x7f, //
    0xf7, 0xff, 0xf7, 0x7f, 0xf7, 0xff, 0xe3, 0x7f, 0xf7, 0x07, 0xe8, 0x7f, //
    0xef, 0xf8, 0x67, 0x70, 0x0f, 0xff, 0xbb, 0x6f, 0xf1, 0x00, 0xd0, 0x5b, //
    0xfd, 0x3f, 0xec, 0x53, 0xc1, 0xff, 0xef, 0x57, 0x9f, 0xfd, 0xee, 0x5f, //
    0x9f, 0xfc, 0xae, 0x5f, 0x1f, 0x78, 0xac, 0x5f, 0x3f, 0x00, 0x50, 0x6c, //
    0x7f, 0x00, 0xdc, 0x77, 0xff, 0xc0, 0x3f, 0x78, 0xff, 0x01, 0xf8, 0x7f, //
    0xff, 0x03, 0x9c, 0x78, 0xff, 0x07, 0x8c, 0x7c, 0xff, 0x0f, 0xce, 0x78, //
    0xff, 0xff, 0xcf, 0x7f, 0xff, 0xff, 0xcf, 0x78, 0xff, 0xff, 0xdf, 0x78, //
    0xff, 0xff, 0xdf, 0x7d, 0xff, 0xff, 0x3f, 0x7e, 0xff, 0xff, 0xff, 0x7f,
];

const BYTES_PER_ROW: usize = ARMIN_WIDTH.div_ceil(8);
/// Half-block rendering: one grid row packs two bitmap rows.
const DISPLAY_HEIGHT: usize = ARMIN_HEIGHT.div_ceil(2);

const EFFECTS: [Effect; 7] = [
    Effect::Typewriter,
    Effect::Scanline,
    Effect::Rain,
    Effect::Fade,
    Effect::Crt,
    Effect::Glitch,
    Effect::Dissolve,
];

/// The animation variants. `EFFECTS` (`armin.js:22`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Typewriter,
    Scanline,
    Rain,
    Fade,
    Crt,
    Glitch,
    Dissolve,
}

impl Effect {
    /// Frames per grid cell the reveal effects move (the `pixelsPerFrame`
    /// constants of `tickTypewriter`, `tickFade`, `tickDissolve`).
    fn reveal_step(self) -> usize {
        match self {
            Effect::Fade => 15,
            Effect::Dissolve => 20,
            _ => 3,
        }
    }

    /// Frames per second (`startAnimation`, `armin.js:162`).
    fn fps(self) -> u64 {
        match self {
            Effect::Glitch => 60,
            _ => 30,
        }
    }
}

/// Get pixel at (x, y): true = foreground, false = background
/// (`isArminPixel`, `armin.js:24-30`). Out-of-range reads are background,
/// which is also what JS's `undefined >> bit` (0) produces past the bitmap.
pub fn is_armin_pixel(x: usize, y: usize) -> bool {
    if y >= ARMIN_HEIGHT || x >= ARMIN_WIDTH {
        return false;
    }
    let byte = BITS[y * BYTES_PER_ROW + x / 8];
    (byte >> (x % 8)) & 1 == 0
}

/// The character for a cell (2 vertical pixels packed) (`getChar`).
fn cell_char(x: usize, row: usize) -> char {
    let upper = is_armin_pixel(x, row * 2);
    let lower = is_armin_pixel(x, row * 2 + 1);
    match (upper, lower) {
        (true, true) => '█',
        (true, false) => '▀',
        (false, true) => '▄',
        (false, false) => ' ',
    }
}

/// The final image grid (`buildFinalGrid`).
fn final_grid() -> Vec<Vec<char>> {
    (0..DISPLAY_HEIGHT)
        .map(|row| (0..ARMIN_WIDTH).map(|x| cell_char(x, row)).collect())
        .collect()
}

fn empty_grid() -> Vec<Vec<char>> {
    vec![vec![' '; ARMIN_WIDTH]; DISPLAY_HEIGHT]
}

/// Seeded split-mix64: upstream's `Math.random` at construction and per
/// frame. Deterministic per `(seed, call)`, random per run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A float in `0..1`, like `Math.random()`.
    fn unit(&mut self) -> f64 {
        self.next() as f64 / u64::MAX as f64
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.unit() * bound as f64) as usize
    }
}

/// One rain column: the drop starts `start` frames above the grid and
/// re-enters `bounce` frames after settling (`tickRain`, `armin.js:222-262`).
struct Drop {
    start: isize,
    bounce: isize,
}

/// The inline fallback when the 3D overlay cannot play: the animated bitmap
/// rendered into the chat. `ArminComponent` (`armin.js:55-334`).
pub struct ArminComponent {
    start: Instant,
    /// Per-instance randomness seed; the glitch effect re-seeds it per frame.
    seed: u64,
    effect: Effect,
    fps: u64,
    /// Per-cell reveal order for the shuffle effects (fade, dissolve).
    positions: Vec<(usize, usize)>,
    /// Initial noise characters for the dissolve effect.
    noise: Vec<Vec<char>>,
    drops: Vec<Drop>,
    final_grid: Vec<Vec<char>>,
}

impl ArminComponent {
    pub fn new() -> Self {
        // The construction seed stands in for Math.random; a nanosecond
        // timestamp is as unpredictable as upstream's source.
        let mut random = Random(fresh_seed());
        let effect = EFFECTS[random.below(EFFECTS.len())];
        let mut component = Self {
            start: Instant::now(),
            seed: random.0,
            effect,
            fps: effect.fps(),
            positions: Vec::new(),
            noise: Vec::new(),
            drops: Vec::new(),
            final_grid: final_grid(),
        };
        component.init_effect(&mut random);
        component
    }

    /// `initEffect` (`armin.js:100-160`): the random per-effect layout.
    fn init_effect(&mut self, random: &mut Random) {
        match self.effect {
            Effect::Fade | Effect::Dissolve => {
                let mut positions: Vec<(usize, usize)> = (0..DISPLAY_HEIGHT)
                    .flat_map(|row| (0..ARMIN_WIDTH).map(move |x| (row, x)))
                    .collect();
                // Fisher-Yates shuffle (`initEffect`'s fade arm).
                for i in (1..positions.len()).rev() {
                    let j = random.below(i + 1);
                    positions.swap(i, j);
                }
                self.positions = positions;
                if self.effect == Effect::Dissolve {
                    let chars = [' ', '░', '▒', '▓', '█', '▀', '▄'];
                    self.noise = (0..DISPLAY_HEIGHT)
                        .map(|_| {
                            (0..ARMIN_WIDTH)
                                .map(|_| chars[random.below(chars.len())])
                                .collect()
                        })
                        .collect();
                }
            }
            Effect::Rain => {
                self.drops = (0..ARMIN_WIDTH)
                    .map(|_| Drop {
                        start: -(random.below(DISPLAY_HEIGHT * 2) as isize),
                        bounce: -(random.below(5) as isize) - 1,
                    })
                    .collect();
            }
            _ => {}
        }
    }

    /// The grid at `frame`, computed from scratch — the effects are pure
    /// functions of the frame index, and past their end every effect draws
    /// the final image.
    fn grid_at(&self, frame: usize) -> Vec<Vec<char>> {
        match self.effect {
            Effect::Typewriter => {
                let mut grid = empty_grid();
                let revealed = frame * 3;
                for index in 0..revealed.min(ARMIN_WIDTH * DISPLAY_HEIGHT) {
                    let row = index / ARMIN_WIDTH;
                    let x = index % ARMIN_WIDTH;
                    grid[row][x] = self.final_grid[row][x];
                }
                grid
            }
            Effect::Scanline => {
                let mut grid = empty_grid();
                for row in 0..frame.min(DISPLAY_HEIGHT) {
                    grid[row].copy_from_slice(&self.final_grid[row]);
                }
                grid
            }
            Effect::Rain => self.rain_grid(frame),
            Effect::Fade => {
                let mut grid = empty_grid();
                for &(row, x) in self.positions.iter().take(frame * self.effect.reveal_step()) {
                    grid[row][x] = self.final_grid[row][x];
                }
                grid
            }
            Effect::Dissolve => {
                // Start from noise; the shuffled reveal resolves into the image.
                let mut grid = self.noise.clone();
                for &(row, x) in self.positions.iter().take(frame * self.effect.reveal_step()) {
                    grid[row][x] = self.final_grid[row][x];
                }
                grid
            }
            Effect::Crt => {
                let mut grid = empty_grid();
                let mid_row = DISPLAY_HEIGHT / 2;
                let top = mid_row.saturating_sub(frame);
                let bottom = mid_row + frame;
                for row in top..=bottom.min(DISPLAY_HEIGHT - 1) {
                    grid[row].copy_from_slice(&self.final_grid[row]);
                }
                grid
            }
            Effect::Glitch => self.glitch_grid(frame),
        }
    }

    /// `tickRain` (`armin.js:222-262`) replayed to `frame`. The state
    /// (settled counters, falling positions) is rebuilt each call and the
    /// grid is redrawn per iteration, so transient drops leave no trail —
    /// the replay breaks once every column has settled. Columns without any
    /// art (the bitmap's first and last) count as settled, which upstream's
    /// interval does not require but changes nothing on screen.
    fn rain_grid(&self, frame: usize) -> Vec<Vec<char>> {
        let mut grid = empty_grid();
        let mut settled: Vec<usize> = (0..ARMIN_WIDTH)
            .map(|x| {
                if self.final_grid.iter().any(|row| row[x] != ' ') {
                    0
                } else {
                    // Nothing to rain: done from the start.
                    DISPLAY_HEIGHT
                }
            })
            .collect();
        let mut y: Vec<isize> = self.drops.iter().map(|drop| drop.start).collect();
        for _ in 0..=frame {
            let mut all_settled = true;
            for x in 0..ARMIN_WIDTH {
                let previous_y = y[x];
                if settled[x] < DISPLAY_HEIGHT {
                    all_settled = false;
                    y[x] += 1;
                    // Target row: the lowest non-space pixel above the
                    // settled top; settle when the drop reaches it.
                    let mut target_row: isize = -1;
                    for row in (0..DISPLAY_HEIGHT - settled[x]).rev() {
                        if self.final_grid[row][x] != ' ' {
                            target_row = row as isize;
                            break;
                        }
                    }
                    if target_row >= 0 && y[x] >= target_row {
                        settled[x] = DISPLAY_HEIGHT - target_row as usize;
                        y[x] = self.drops[x].bounce;
                    }
                }
                // Draw settled pixels, bottom-up.
                for row in (DISPLAY_HEIGHT - settled[x].min(DISPLAY_HEIGHT))..DISPLAY_HEIGHT {
                    grid[row][x] = self.final_grid[row][x];
                }
                // Clear the drop's previous cell above the settled region so
                // only the current falling position shows, like upstream's
                // per-tick grid rebuild.
                if (0..DISPLAY_HEIGHT as isize).contains(&previous_y) {
                    let row = previous_y as usize;
                    if row < DISPLAY_HEIGHT - settled[x].min(DISPLAY_HEIGHT) {
                        grid[row][x] = ' ';
                    }
                }
                // Draw the falling drop.
                if settled[x] < DISPLAY_HEIGHT
                    && (0..DISPLAY_HEIGHT as isize).contains(&y[x])
                    && (y[x] as usize) < DISPLAY_HEIGHT - settled[x]
                {
                    grid[y[x] as usize][x] = '▓';
                }
            }
            if all_settled {
                break;
            }
        }
        grid
    }

    /// `tickGlitch` (`armin.js:290-315`): eight corrupted frames, then the
    /// clean image. Corruption is seeded per frame so it shimmers like
    /// upstream's random offsets.
    fn glitch_grid(&self, frame: usize) -> Vec<Vec<char>> {
        if frame >= 8 {
            return self.final_grid.clone();
        }
        let mut random = Random((self.seed ^ (frame as u64).wrapping_mul(7_919)) | 1);
        self.final_grid
            .iter()
            .map(|row| {
                let offset = random.below(7) as isize - 3;
                if random.unit() < 0.3 {
                    // Random horizontal offset: `slice(offset).concat(slice(0,
                    // offset))` is a rotation, never shorter than the row.
                    let mut shifted = row.clone();
                    shifted.rotate_left(offset.rem_euclid(row.len() as isize) as usize);
                    return shifted;
                }
                if random.unit() < 0.2 {
                    // Random vertical swap.
                    let swap_row = random.below(DISPLAY_HEIGHT);
                    return self.final_grid[swap_row].clone();
                }
                row.clone()
            })
            .collect()
    }
}

/// A nanosecond timestamp as the per-instance randomness seed.
fn fresh_seed() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x5eed)
        | 1
}

impl Default for ArminComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for ArminComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let elapsed_frames = (self.start.elapsed().as_millis() as u64 * self.fps / 1000) as usize;
        let grid = self.grid_at(elapsed_frames);
        let padding = 1;
        let available_width = width.saturating_sub(padding);
        let accent = theme::theme();
        let mut lines: Vec<String> = grid
            .iter()
            .map(|row| {
                // Clip row to available width before applying color.
                let clipped: String = row.iter().take(available_width).collect();
                let pad_right = width.saturating_sub(padding + clipped.chars().count());
                let colored = accent.fg(ThemeColor::Accent, &clipped);
                format!(" {colored}{}", " ".repeat(pad_right))
            })
            .collect();
        // Add "ARMIN SAYS HI" at the end (`render`, `armin.js:89-92`).
        let message = "ARMIN SAYS HI";
        let colored = accent.fg(ThemeColor::Accent, message);
        let pad_right = width.saturating_sub(padding + message.len());
        lines.push(format!(" {colored}{}", " ".repeat(pad_right)));
        lines
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::{strip_terminal_sequences, visible_width};

    /// A constructor that pins the effect and seed, so tests can exercise
    /// each effect deterministically.
    fn with_effect(effect: Effect, seed: u64) -> ArminComponent {
        let mut component = ArminComponent {
            start: Instant::now(),
            seed,
            effect,
            fps: effect.fps(),
            positions: Vec::new(),
            noise: Vec::new(),
            drops: Vec::new(),
            final_grid: final_grid(),
        };
        let mut random = Random(seed | 1);
        component.init_effect(&mut random);
        component
    }

    #[test]
    fn the_bitmap_probes_match_the_xbm() {
        // The XBM is inverted (1=background): row 0 is 0xff 0xff 0xff 0x7f,
        // all background; the art starts at row 1, columns 8..12.
        assert!(!is_armin_pixel(0, 0));
        assert!(!is_armin_pixel(23, 0));
        assert!(is_armin_pixel(8, 1));
        assert!(is_armin_pixel(11, 1));
        assert!(!is_armin_pixel(7, 1));
        assert!(!is_armin_pixel(31, 0), "past the width is background");
        assert!(!is_armin_pixel(0, 36), "past the height is background");
        // The artwork has 245 foreground pixels (validated against upstream).
        let foreground = (0..ARMIN_HEIGHT)
            .flat_map(|y| (0..ARMIN_WIDTH).map(move |x| (x, y)))
            .filter(|&(x, y)| is_armin_pixel(x, y))
            .count();
        assert_eq!(foreground, 245);
    }

    #[test]
    fn the_final_grid_packs_two_rows_per_cell() {
        let grid = final_grid();
        assert_eq!(grid.len(), 18);
        assert_eq!(grid[0].len(), ARMIN_WIDTH);
        for (row, line) in grid.iter().enumerate() {
            for (x, &char) in line.iter().enumerate() {
                let expected = cell_char(x, row);
                assert_eq!(char, expected, "cell ({x}, {row})");
            }
        }
    }

    #[test]
    fn every_effect_reaches_the_final_image() {
        // The rain settles within ~1500 frames; give it plenty of headroom.
        for effect in EFFECTS {
            let component = with_effect(effect, 0x5eed);
            let grid = component.grid_at(10_000);
            for (row, line) in grid.iter().enumerate() {
                assert_eq!(line, &component.final_grid[row], "{effect:?} row {row}");
            }
        }
    }

    #[test]
    fn effects_start_from_their_special_first_frames() {
        // Scanline reveals one row per frame; frame 1 shows only the first.
        let scanline = with_effect(Effect::Scanline, 7);
        let grid = scanline.grid_at(1);
        assert!(grid[0].iter().any(|&char| char != ' '));
        assert!(grid[1].iter().all(|&char| char == ' '));
        // Glitch's first frame is corrupted; its eighth is clean.
        let glitch = with_effect(Effect::Glitch, 7);
        assert_ne!(glitch.grid_at(0), glitch.final_grid);
        assert_eq!(glitch.grid_at(8), glitch.final_grid);
        // Typewriter reveals three cells per frame; at frame 30 the first 90
        // cells are in, of which 12 hold art (validated against the bitmap).
        let typewriter = with_effect(Effect::Typewriter, 7);
        let grid = typewriter.grid_at(30);
        let revealed = grid
            .iter()
            .flat_map(|row| row.iter())
            .filter(|&&char| char != ' ')
            .count();
        assert_eq!(revealed, 12);
        // CRT expands from the middle.
        let crt = with_effect(Effect::Crt, 7);
        let grid = crt.grid_at(2);
        assert!(grid[DISPLAY_HEIGHT / 2].iter().any(|&char| char != ' '));
        assert!(grid[0].iter().all(|&char| char == ' '));
        // Rain shows at most one falling drop per column — no transient
        // trail above the settled region.
        let rain = with_effect(Effect::Rain, 0x5eed);
        let early = rain.grid_at(30);
        for x in 0..ARMIN_WIDTH {
            let drops = early.iter().filter(|row| row[x] == '▓').count();
            assert!(drops <= 1, "column {x} shows {drops} drops");
        }
        // And by frame 200 some pixels have settled.
        let later = rain.grid_at(200);
        let settled_cells = later
            .iter()
            .flat_map(|row| row.iter())
            .filter(|&&char| char != ' ' && char != '▓')
            .count();
        assert!(settled_cells > 0, "pixels have settled by frame 200");
    }

    #[test]
    fn render_colors_the_art_and_appends_the_message() {
        // Render structure only: the frame depends on the wall clock, and the
        // art content is covered by the grid tests above.
        let component = with_effect(Effect::Scanline, 7);
        let lines = component.render(60);
        assert_eq!(lines.len(), DISPLAY_HEIGHT + 1);
        let last = strip_terminal_sequences(&lines[lines.len() - 1]);
        assert!(last.starts_with(' '));
        assert!(last.contains("ARMIN SAYS HI"));
        // Every line fits the width.
        for line in &lines {
            assert!(visible_width(line) <= 60);
        }
    }
}

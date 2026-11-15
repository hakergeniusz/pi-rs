//! Oklab and OKHSL <-> sRGB conversion.
//!
//! Ground truth: `packages/tui/src/oklab.ts` (233 lines). Björn Ottosson's
//! reference implementation (MIT), as pi vendors it; the header comment and
//! the license terms travel with it.
//!
//! `colors.rs` builds OKLCH/OKHSL colors, gamut mapping and mixing on this,
//! so the theme colors the renderer emits match pi's byte for byte.

// Color-space constants mirror the TS table digit-for-digit; digit grouping
// would break copy-comparability.
#![allow(clippy::unreadable_literal)]
/// sRGB channels, 0-255. `terminal-colors.ts:1-5`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb {
    /// Red channel, 0-255.
    pub r: f64,
    /// Green channel, 0-255.
    pub g: f64,
    /// Blue channel, 0-255.
    pub b: f64,
}

type Vector = [f64; 3];
type Matrix = [Vector; 3];

const fn multiply(m: Matrix, v: Vector) -> Vector {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

// `oklab.ts:29-48`.
const LINEAR_SRGB_TO_LMS: Matrix = [
    [0.4122214694707629, 0.5363325372617349, 0.0514459932675022],
    [0.2119034958178251, 0.6806995506452344, 0.1073969535369405],
    [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
];
const LMS_TO_LAB: Matrix = [
    [0.210454268309314, 0.793617774702305, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
const LAB_TO_LMS: Matrix = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
const LMS_TO_LINEAR_SRGB: Matrix = [
    [
        4.076_741_636_075_958,
        -3.307_711_539_258_063,
        0.2309699031821043,
    ],
    [
        -1.2684379732850315,
        2.609_757_349_287_688,
        -0.341319376002657,
    ],
    [-0.0041960761386756, -0.7034186179359362, 1.7076146940746117],
];

/// Per sRGB channel: the (a, b) half-plane where that channel clips first and
/// the polynomial approximating the maximum saturation there. `oklab.ts:53-66`.
const SATURATION_FIT: [([f64; 2], [f64; 5]); 3] = [
    (
        [-1.8817031, -0.80936501],
        [1.19086277, 1.76576728, 0.59662641, 0.75515197, 0.56771245],
    ),
    (
        [1.8144408, -1.19445267],
        [0.73956515, -0.45954404, 0.08285427, 0.12541073, -0.14503204],
    ),
    (
        [0.13110758, 1.81333971],
        [1.35733652, -0.00915799, -1.1513021, -0.50559606, 0.00692167],
    ),
];

const K1: f64 = 0.206;
const K2: f64 = 0.03;

fn k3() -> f64 {
    (1.0 + K1) / (1.0 + K2)
}

/// Oklab lightness to OKHSL lightness. `oklab.ts:72-73`.
// Ported verbatim; `f64::midpoint` is stable only since 1.87 and the
// workspace MSRV is 1.85.
#[allow(clippy::manual_midpoint)]
pub fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    let k3 = k3();
    0.5 * (k3 * x - K1 + ((k3 * x - K1).powi(2) + 4.0 * K2 * k3 * x).sqrt())
}

/// OKHSL lightness to Oklab lightness. `oklab.ts:75`.
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    let k3 = k3();
    (x * x + K1 * x) / (k3 * (x + K2))
}

/// linear -> encoded sRGB channel, both 0-1. `oklab.ts:78-79`.
fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.0031308 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

/// encoded -> linear sRGB channel, both 0-1. `oklab.ts:81`.
fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Oklab `[L, a, b]` to linear sRGB, which may leave the gamut.
/// `oklab.ts:84-86`.
pub fn oklab_to_linear_srgb(lab: Vector) -> Vector {
    // LAB_TO_LMS gives cube-rooted LMS; the inverse path cubes it again.
    let cubed: Vector = multiply(LAB_TO_LMS, lab).map(|value| value.powi(3));
    multiply(LMS_TO_LINEAR_SRGB, cubed)
}

/// Linear sRGB to Oklab `[L, a, b]`. `oklab.ts:89-91`.
fn linear_srgb_to_oklab(rgb: Vector) -> Vector {
    let cbrt: Vector = multiply(LINEAR_SRGB_TO_LMS, rgb).map(f64::cbrt);
    multiply(LMS_TO_LAB, cbrt)
}

/// sRGB channels (0-255) to Oklab. `oklab.ts:94-96`.
pub fn rgb_to_oklab(rgb: Rgb) -> Vector {
    linear_srgb_to_oklab([
        srgb_to_linear(rgb.r / 255.0),
        srgb_to_linear(rgb.g / 255.0),
        srgb_to_linear(rgb.b / 255.0),
    ])
}

/// Linear sRGB to sRGB channels, clipping out-of-gamut channels.
/// `oklab.ts:99-102`.
pub fn linear_srgb_to_rgb(linear: Vector) -> Rgb {
    let channel = |value: f64| (linear_to_srgb(value).clamp(0.0, 1.0) * 255.0).round();
    Rgb {
        r: channel(linear[0]),
        g: channel(linear[1]),
        b: channel(linear[2]),
    }
}

/// d(cube-root LMS)/d(chroma) along `(a, b)`. `oklab.ts:105-107`.
fn lms_slopes(a: f64, b: f64) -> Vector {
    [
        LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
        LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
        LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
    ]
}

fn dot(weights: Vector, values: Vector) -> f64 {
    weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2]
}

/// Largest saturation (C/L) inside sRGB for hue `(a, b)`: polynomial fit plus
/// one Halley step. `oklab.ts:110-123`.
fn max_saturation(a: f64, b: f64) -> f64 {
    let channel = SATURATION_FIT
        .iter()
        .position(|entry| entry.0[0] * a + entry.0[1] * b > 1.0)
        .unwrap_or(2);
    let k = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k[0] + k[1] * a + k[2] * b + k[3] * a * a + k[4] * a * b;

    let slopes = lms_slopes(a, b);
    let base: Vector = [
        1.0 + saturation * slopes[0],
        1.0 + saturation * slopes[1],
        1.0 + saturation * slopes[2],
    ];
    let cubes: Vector = base.map(|value| value.powi(3));
    let f = dot(weights, cubes);
    let first: Vector = [
        3.0 * slopes[0] * base[0].powi(2),
        3.0 * slopes[1] * base[1].powi(2),
        3.0 * slopes[2] * base[2].powi(2),
    ];
    let f1 = dot(weights, first);
    let second: Vector = [
        6.0 * slopes[0].powi(2) * base[0],
        6.0 * slopes[1].powi(2) * base[1],
        6.0 * slopes[2].powi(2) * base[2],
    ];
    let f2 = dot(weights, second);
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// Lightness and chroma of the most saturated sRGB color of hue `(a, b)`.
/// `oklab.ts:126-130`.
fn cusp(a: f64, b: f64) -> (f64, f64) {
    let saturation = max_saturation(a, b);
    let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let lightness = (1.0 / linear[0].max(linear[1]).max(linear[2])).cbrt();
    (lightness, lightness * saturation)
}

/// Chroma where the constant-lightness line leaves the sRGB gamut.
/// `oklab.ts:133-149`.
// Short names (`a`, `b`, `f`, `f1`, `f2`, `t`, ...) mirror upstream
// oklab.ts color math.
#[allow(clippy::many_single_char_names)]
fn max_chroma(a: f64, b: f64, lightness: f64, cusp: (f64, f64)) -> f64 {
    let (cusp_l, cusp_c) = cusp;
    if lightness <= cusp_l {
        return (cusp_c * lightness) / cusp_l;
    }
    let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms: Vector = [
        lightness + t * slopes[0],
        lightness + t * slopes[1],
        lightness + t * slopes[2],
    ];
    let cubes: Vector = lms.map(|value| value.powi(3));
    let first: Vector = [
        3.0 * slopes[0] * lms[0].powi(2),
        3.0 * slopes[1] * lms[1].powi(2),
        3.0 * slopes[2] * lms[2].powi(2),
    ];
    let second: Vector = [
        6.0 * slopes[0].powi(2) * lms[0],
        6.0 * slopes[1].powi(2) * lms[1],
        6.0 * slopes[2].powi(2) * lms[2],
    ];
    let mut steps: Vec<f64> = Vec::with_capacity(3);
    for row in LMS_TO_LINEAR_SRGB {
        let f = dot(row, cubes) - 1.0;
        let f1 = dot(row, first);
        let f2 = dot(row, second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        steps.push(if u >= 0.0 { -f * u } else { f64::MAX });
    }
    t + steps.iter().copied().fold(f64::INFINITY, f64::min)
}

/// Chroma reference points `[c0, cMid, cMax]` at lightness `l` and hue
/// `(a, b)`. `oklab.ts:152-168`.
fn chroma_stops(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max / (l * (peak.1 / peak.0)).min((1.0 - l) * (peak.1 / (1.0 - peak.0)));
    let mid_s = 0.11516993
        + 1.0
            / (7.4477897
                + 4.1590124 * b
                + a * (-2.19557347
                    + 1.75198401 * b
                    + a * (-2.13704948 - 10.02301043 * b
                        + a * (-4.24894561 + 5.38770819 * b + 4.69891013 * a))));
    let mid_t = 0.11239642
        + 1.0
            / (1.6132032 - 0.68124379 * b
                + a * (0.40370612
                    + 0.90148123 * b
                    + a * (-0.27087943
                        + 0.6122399 * b
                        + a * (0.00299215 - 0.45399568 * b - 0.14661872 * a))));
    let c_mid = 0.9
        * k
        * (1.0 / (1.0 / (l * mid_s).powi(4) + 1.0 / ((1.0 - l) * mid_t).powi(4)))
            .sqrt()
            .sqrt();
    let c_0 = (1.0 / (1.0 / (l * 0.4).powi(2) + 1.0 / ((1.0 - l) * 0.8).powi(2))).sqrt();
    (c_0, c_mid, c_max)
}

/// Convert OKHSL to sRGB channels. `oklab.ts:187-211`.
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab: Vector = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = 2.0 * std::f64::consts::PI * (((hue % 360.0) + 360.0) % 360.0) / 360.0;
        let a = angle.cos();
        let b = angle.sin();
        let (c_0, c_mid, c_max) = chroma_stops(l, a, b);
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c_0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid.powi(2) * 1.25f64.powi(2)) / c_0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

/// OKHSL channels: hue in degrees, saturation and lightness 0-1.
/// `colors.ts:38-42`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Okhsl {
    /// Hue in degrees.
    pub h: f64,
    /// Saturation, 0-1.
    pub s: f64,
    /// Lightness, 0-1.
    pub l: f64,
}

/// Convert sRGB channels to OKHSL. `oklab.ts:215-232`.
pub fn rgb_to_okhsl(rgb: Rgb) -> Okhsl {
    let [l, lab_a, lab_b] = rgb_to_oklab(rgb);
    let chroma = lab_a.hypot(lab_b);
    let lightness = oklab_to_okhsl_lightness(l);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return Okhsl {
            h: 0.0,
            s: 0.0,
            l: lightness,
        };
    }
    let hue = ((lab_b.atan2(lab_a) * 180.0) / std::f64::consts::PI + 360.0) % 360.0;
    let (c_0, c_mid, c_max) = chroma_stops(l, lab_a / chroma, lab_b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c_0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = (0.2 * c_mid.powi(2) * 1.25f64.powi(2)) / c_0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    Okhsl {
        h: hue,
        s: saturation.clamp(0.0, 1.0),
        l: lightness,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, epsilon: f64) -> bool {
        (a - b).abs() <= epsilon
    }

    #[test]
    fn white_and_black_round_trip() {
        let white = oklab_to_linear_srgb([1.0, 0.0, 0.0]);
        let rgb = linear_srgb_to_rgb(white);
        assert_eq!((rgb.r, rgb.g, rgb.b), (255.0, 255.0, 255.0));

        let lab = rgb_to_oklab(Rgb {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        });
        assert!(close(lab[0], 0.0, 1e-9));
    }

    #[test]
    fn oklab_of_known_colors() {
        // Mid grey sits at L ~= 0.6, red keeps a positive a axis.
        let grey = rgb_to_oklab(Rgb {
            r: 128.0,
            g: 128.0,
            b: 128.0,
        });
        assert!(close(grey[0], 0.5998, 1e-3));
        let red = rgb_to_oklab(Rgb {
            r: 255.0,
            g: 0.0,
            b: 0.0,
        });
        assert!(close(red[0], 0.6280, 1e-3));
        assert!(red[1] > 0.2);
    }

    #[test]
    fn okhsl_round_trips_through_srgb() {
        for (h, s, l) in [(0.0, 0.5, 0.5), (120.0, 0.8, 0.4), (240.0, 0.6, 0.7)] {
            let rgb = okhsl_to_rgb(h, s, l);
            let back = rgb_to_okhsl(rgb);
            assert!(close(back.h, h, 1.5), "hue {} vs {}", back.h, h);
            assert!(close(back.s, s, 0.05), "sat {} vs {}", back.s, s);
            assert!(close(back.l, l, 0.05), "lightness {} vs {}", back.l, l);
        }
    }

    #[test]
    fn okhsl_edges_are_in_gamut() {
        let white = okhsl_to_rgb(200.0, 1.0, 1.0);
        assert_eq!((white.r, white.g, white.b), (255.0, 255.0, 255.0));
        let black = okhsl_to_rgb(200.0, 1.0, 0.0);
        assert_eq!((black.r, black.g, black.b), (0.0, 0.0, 0.0));
    }

    #[test]
    fn greys_have_no_hue() {
        let grey = rgb_to_okhsl(Rgb {
            r: 128.0,
            g: 128.0,
            b: 128.0,
        });
        assert!(close(grey.s, 0.0, 1e-9));
        assert!(close(grey.h, 0.0, 1e-9));
    }

    #[test]
    fn lightness_conversion_is_monotonic() {
        let mut previous = -1.0;
        for step in 0..=10 {
            let value = step as f64 / 10.0;
            let lightness = oklab_to_okhsl_lightness(value);
            assert!(lightness > previous || step == 0);
            previous = lightness;
        }
    }
}

//! Status-text animations keyed to wall time, so their speed doesn't depend
//! on render rate.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::style::Color;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anim {
    None,
    Shimmer,
    Breathe,
}

/// The shared animation clock, started on first call.
pub fn elapsed() -> Duration {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed()
}

/// Seconds per sweep or breath.
const PERIOD: f32 = 2.0;
const BAND_HALF_WIDTH: f32 = 5.0;
/// Cells beyond each edge, so the band slides fully off instead of
/// snapping back.
const PADDING: usize = 10;

/// How far through its fixed-period cycle an animation is at `elapsed`.
pub fn phase(elapsed: Duration) -> f32 {
    (elapsed.as_secs_f32() % PERIOD) / PERIOD
}

/// The color of character `i` of `len` as a highlight band sweeps past.
pub fn shimmer(base: Color, i: usize, len: usize, elapsed: Duration) -> Color {
    shimmer_at(base, i, len, phase(elapsed))
}

/// The same band `phase` of the way through its sweep, for callers that
/// pace the sweep themselves.
pub fn shimmer_at(base: Color, i: usize, len: usize, phase: f32) -> Color {
    let period_cells = (len + 2 * PADDING) as f32;
    let pos = phase.rem_euclid(1.0) * period_cells;
    let dist = ((i + PADDING) as f32 - pos).abs();
    if dist > BAND_HALF_WIDTH {
        return base;
    }
    let t = 0.5 * (1.0 + (std::f32::consts::PI * dist / BAND_HALF_WIDTH).cos());
    blend(rgb(base), (255, 255, 255), t * 0.9)
}

/// The text's color as it pulses between dim and full intensity.
pub fn breathe(base: Color, elapsed: Duration) -> Color {
    let t = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * phase(elapsed)).cos());
    let (r, g, b) = rgb(base);
    let dim = (r / 3, g / 3, b / 3);
    blend(dim, (r, g, b), t)
}

/// Seconds between a pulsing cell's random brightness targets and glyph
/// reselections.
const PULSE_STEP: f32 = 0.25;
/// Seconds for braille to take over the whole rule once work starts.
const TAKEOVER: f32 = 1.0;
/// Step salts keeping a cell's takeover order and glyph draws independent
/// of its brightness draws.
const ORDER: u64 = 0x9E37_79B9_7F4A_7C15;
const GLYPH: u64 = 0xD1B5_4A32_D192_ED03;

/// The color of rule cell `i` flickering at a random brightness of its
/// own, blending up from `base` toward white, with `energy` scaling every
/// cell from `base` at 0 to the full range at 1.
pub fn flicker(base: Color, i: usize, elapsed: Duration, energy: f32) -> Color {
    let t = elapsed.as_secs_f32() / PULSE_STEP;
    let step = t as u64;
    let frac = t - step as f32;
    let smooth = frac * frac * (3.0 - 2.0 * frac);
    let from = noise(i as u64, step);
    let level = from + (noise(i as u64, step + 1) - from) * smooth;
    blend(rgb(base), (255, 255, 255), energy * level)
}

/// The share of the rule braille has taken over, `age` into the working
/// state.
pub fn takeover(age: Duration) -> f32 {
    (age.as_secs_f32() / TAKEOVER).min(1.0)
}

/// The glyph of rule cell `i`: a dash until the braille `takeover` share
/// reaches it, cells falling in a fixed scattered order, then a braille
/// pattern reselected every step, each cell on its own schedule.
pub fn pulse_glyph(i: usize, elapsed: Duration, takeover: f32) -> char {
    let order = noise(i as u64, ORDER);
    if order >= takeover {
        return '─';
    }
    let step = (elapsed.as_secs_f32() / PULSE_STEP + order) as u64;
    // Skip the blank pattern so every taken cell shows dots.
    let pattern = 1 + (noise(i as u64, step ^ GLYPH) * 255.0) as u32;
    char::from_u32(0x2800 + pattern).expect("braille block")
}

/// A hash of the pair spread evenly over 0..1.
fn noise(cell: u64, step: u64) -> f32 {
    let mut x = cell ^ step.rotate_left(32);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x >> 40) as f32 / (1u64 << 24) as f32
}

fn blend(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let ch = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
    Color::Rgb(ch(from.0, to.0), ch(from.1, to.1), ch(from.2, to.2))
}

/// Blending needs channels, and the terminal default has none.
fn rgb(color: Color) -> (u8, u8, u8) {
    crate::server::palette::rgb(color).unwrap_or((170, 170, 170))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_band_sweeps_with_time() {
        let len = "[working]".len();
        let a: Vec<Color> = (0..len)
            .map(|i| shimmer(Color::Yellow, i, len, Duration::ZERO))
            .collect();
        let b: Vec<Color> = (0..len)
            .map(|i| shimmer(Color::Yellow, i, len, Duration::from_millis(500)))
            .collect();
        assert_ne!(a, b, "the band must move");
    }

    #[test]
    fn shimmer_is_periodic() {
        let len = "[working]".len();
        for i in 0..len {
            assert_eq!(
                shimmer(Color::Yellow, i, len, Duration::from_millis(300)),
                shimmer(Color::Yellow, i, len, Duration::from_millis(2300)),
            );
        }
    }

    #[test]
    fn flicker_varies_by_cell_and_time_from_base_up_toward_white() {
        let at = |ms: u64, energy: f32| -> Vec<Color> {
            (0..20)
                .map(|i| flicker(Color::Red, i, Duration::from_millis(ms), energy))
                .collect()
        };
        let a = at(0, 1.0);
        assert!(a.iter().any(|c| *c != a[0]), "cells must differ");
        assert_ne!(a, at(400, 1.0), "brightness must move");
        assert!(
            a.iter()
                .all(|c| matches!(c, Color::Rgb(r, g, b) if (205..=255).contains(r) && g == b))
        );
        // No energy holds every cell at the base color.
        assert!(at(0, 0.0).iter().all(|c| *c == Color::Rgb(205, 0, 0)));
    }

    #[test]
    fn takeover_completes_after_the_ramp() {
        assert_eq!(takeover(Duration::ZERO), 0.0);
        assert_eq!(takeover(Duration::from_millis(500)), 0.5);
        assert_eq!(takeover(Duration::from_secs(5)), 1.0);
    }

    #[test]
    fn braille_takes_the_rule_over_gradually_then_keeps_changing() {
        let is_braille = |c: &char| ('\u{2801}'..='\u{28FF}').contains(c);
        let at = |ms: u64, takeover: f32| -> Vec<char> {
            (0..40)
                .map(|i| pulse_glyph(i, Duration::from_millis(ms), takeover))
                .collect()
        };
        assert!(at(0, 0.0).iter().all(|c| *c == '─'));
        let half = at(0, 0.5);
        assert!(half.contains(&'─'), "some cells still dashes");
        assert!(half.iter().any(is_braille), "some cells taken");
        // A cell taken stays taken as the share grows.
        let later = at(0, 0.75);
        for (a, b) in half.iter().zip(&later) {
            assert!(*a == '─' || is_braille(b));
        }
        let full = at(0, 1.0);
        assert!(full.iter().all(is_braille));
        assert_ne!(full, at(1000, 1.0), "patterns must be reselected");
    }

    #[test]
    fn breathe_pulses_between_dim_and_full() {
        assert_eq!(breathe(Color::Red, Duration::ZERO), Color::Rgb(68, 0, 0));
        assert_eq!(
            breathe(Color::Red, Duration::from_secs(1)),
            Color::Rgb(205, 0, 0)
        );
        assert_eq!(
            breathe(Color::Red, Duration::from_millis(500)),
            breathe(Color::Red, Duration::from_millis(1500)),
        );
    }
}

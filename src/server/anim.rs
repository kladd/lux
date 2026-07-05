//! Time-driven status-text animations (REQ-UI-005/006), modeled on
//! Codex's shimmer (`~/src/codex/codex-rs/tui/src/shimmer.rs`): a
//! highlight band sweeping across the text for shimmer, an intensity
//! pulse for breathing, both keyed off elapsed wall time rather than
//! frame count so their speed doesn't depend on render rate.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::style::Color;

/// How a status text's color moves over time (REQ-UI-005/006/007).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anim {
    None,
    Shimmer,
    Breathe,
}

/// Elapsed time since the server's first frame: the shared clock every
/// animation phase derives from.
pub fn elapsed() -> Duration {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed()
}

/// Seconds per shimmer sweep and per breath.
const PERIOD: f32 = 2.0;
/// The band fades over this many cells to either side of its center.
const BAND_HALF_WIDTH: f32 = 5.0;
/// Off-text lead-in/out cells, so the band slides in from beyond one edge
/// and fully exits past the other instead of wrapping abruptly.
const PADDING: usize = 10;

/// The shimmer band's color for character `i` of `len` (REQ-UI-005): a
/// raised-cosine highlight band sweeping the text, blending the state
/// color toward white under the band's center.
pub fn shimmer(base: Color, i: usize, len: usize, elapsed: Duration) -> Color {
    let period_cells = (len + 2 * PADDING) as f32;
    let pos = (elapsed.as_secs_f32() % PERIOD) / PERIOD * period_cells;
    let dist = ((i + PADDING) as f32 - pos).abs();
    if dist > BAND_HALF_WIDTH {
        return base;
    }
    let t = 0.5 * (1.0 + (std::f32::consts::PI * dist / BAND_HALF_WIDTH).cos());
    blend(rgb(base), (255, 255, 255), t * 0.9)
}

/// The whole text's color mid-breath (REQ-UI-006): a raised-cosine pulse
/// between the state color's dimmed and full intensity.
pub fn breathe(base: Color, elapsed: Duration) -> Color {
    let phase = (elapsed.as_secs_f32() % PERIOD) / PERIOD;
    let t = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * phase).cos());
    let (r, g, b) = rgb(base);
    let dim = (r / 3, g / 3, b / 3);
    blend(dim, (r, g, b), t)
}

fn blend(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let ch = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
    Color::Rgb(ch(from.0, to.0), ch(from.1, to.1), ch(from.2, to.2))
}

/// The xterm palette value for the named colors status text uses; the
/// blend needs concrete channels.
fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Yellow => (205, 205, 0),
        Color::Red => (205, 0, 0),
        Color::Green => (0, 205, 0),
        Color::DarkGray => (128, 128, 128),
        _ => (170, 170, 170),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_band_sweeps_with_time() {
        let len = "[working]".len();
        // Far from the band, the base color is untouched; as the band
        // reaches a cell its color lifts toward white, so two instants a
        // quarter-period apart style some cell differently.
        let a: Vec<Color> =
            (0..len).map(|i| shimmer(Color::Yellow, i, len, Duration::ZERO)).collect();
        let b: Vec<Color> =
            (0..len).map(|i| shimmer(Color::Yellow, i, len, Duration::from_millis(500))).collect();
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
    fn breathe_pulses_between_dim_and_full() {
        // Phase 0 is the dim end, half-period the full state color.
        assert_eq!(breathe(Color::Red, Duration::ZERO), Color::Rgb(68, 0, 0));
        assert_eq!(breathe(Color::Red, Duration::from_secs(1)), Color::Rgb(205, 0, 0));
        assert_eq!(
            breathe(Color::Red, Duration::from_millis(500)),
            breathe(Color::Red, Duration::from_millis(1500)),
        );
    }
}

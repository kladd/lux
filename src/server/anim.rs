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

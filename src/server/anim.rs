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

/// The color of character `i` of `len` as a highlight band sweeps past.
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

/// The text's color as it pulses between dim and full intensity.
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

/// Concrete channels for the named colors, since blending needs them.
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

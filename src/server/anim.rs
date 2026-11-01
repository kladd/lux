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
pub const PERIOD: f32 = 2.0;
const MIN_BAND_HALF_WIDTH: f32 = 3.0;
/// Cells beyond each edge, so the band slides fully off instead of
/// snapping back.
const PADDING: usize = 10;

/// How far through its fixed-period cycle an animation is at `elapsed`.
pub fn phase(elapsed: Duration) -> f32 {
    (elapsed.as_secs_f32() % PERIOD) / PERIOD
}

/// The color of character `i` of `len`, drawn over `bg`, as a band sweeps
/// past.
pub fn shimmer(base: Color, bg: Color, i: usize, len: usize, elapsed: Duration) -> Color {
    shimmer_at(base, bg, i, len, phase(elapsed))
}

/// The same band `phase` of the way through its sweep, for callers that
/// pace the sweep themselves. Text outside the band sits halfway into `bg`.
pub fn shimmer_at(base: Color, bg: Color, i: usize, len: usize, phase: f32) -> Color {
    let half_width = (len as f32 * 0.1).max(MIN_BAND_HALF_WIDTH);
    let padding = PADDING.max(half_width.ceil() as usize);
    let period_cells = (len + 2 * padding) as f32;
    let pos = phase.rem_euclid(1.0) * period_cells;
    let dist = ((i + padding) as f32 - pos).abs().min(half_width);
    let t = 0.5 * (1.0 + (std::f32::consts::PI * dist / half_width).cos());
    let bg = crate::server::palette::rgb(bg).unwrap_or((0, 0, 0));
    blend(bg, rgb(base), 0.5 + 0.5 * t)
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
            .map(|i| shimmer(Color::Yellow, Color::Black, i, len, Duration::ZERO))
            .collect();
        let b: Vec<Color> = (0..len)
            .map(|i| {
                shimmer(
                    Color::Yellow,
                    Color::Black,
                    i,
                    len,
                    Duration::from_millis(500),
                )
            })
            .collect();
        assert_ne!(a, b, "the band must move");
    }

    #[test]
    fn shimmer_is_periodic() {
        let len = "[working]".len();
        for i in 0..len {
            assert_eq!(
                shimmer(
                    Color::Yellow,
                    Color::Black,
                    i,
                    len,
                    Duration::from_millis(300)
                ),
                shimmer(
                    Color::Yellow,
                    Color::Black,
                    i,
                    len,
                    Duration::from_millis(2300)
                ),
            );
        }
    }

    #[test]
    fn shimmer_dims_text_outside_the_band_into_the_background() {
        let len = "[working]".len();
        assert_eq!(
            shimmer_at(Color::Yellow, Color::Black, 0, len, 0.5),
            Color::Rgb(102, 102, 0)
        );
        assert_eq!(
            shimmer_at(Color::Yellow, Color::White, 0, len, 0.5),
            Color::Rgb(230, 230, 127)
        );
    }

    #[test]
    fn shimmer_band_reaches_full_color_at_its_center() {
        let len = "[working]".len();
        let center = shimmer_at(Color::Yellow, Color::Black, 4, len, 14.0 / 29.0);
        assert_eq!(center, Color::Rgb(205, 205, 0));
    }

    #[test]
    fn shimmer_band_scales_with_text_width() {
        let lit = |len: usize, phase: f32| {
            let dim = shimmer_at(Color::Yellow, Color::Black, usize::MAX / 2, len, phase);
            (0..len)
                .filter(|&i| shimmer_at(Color::Yellow, Color::Black, i, len, phase) != dim)
                .count()
        };
        assert_eq!(
            lit(9, 14.5 / 29.0),
            6,
            "a short label gets the minimum band"
        );
        assert_eq!(
            lit(100, 60.5 / 120.0),
            20,
            "a tenth of the width either side"
        );
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

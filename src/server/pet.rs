//! The pet: a sixel or kitty graphics sprite whose mood follows the agents.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::server::agent::Status;

type Rgb = (u8, u8, u8);

/// The sprite's size in pixels, drawn for 9×18 cells.
pub const WIDTH: usize = 72;
pub const HEIGHT: usize = 36;
/// The cells the pet covers.
pub const COLS: u16 = 8;
pub const ROWS: u16 = 2;
/// How long the pet takes to fade back in after stepping aside.
pub const FADE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mood {
    Idle,
    Waiting,
    Working,
    Done,
    Blocked,
}

/// The most urgent state among the agents, or `None` with no agents.
pub fn mood(statuses: impl IntoIterator<Item = Status>) -> Option<Mood> {
    statuses
        .into_iter()
        .map(|status| match status {
            Status::Idle => Mood::Idle,
            Status::Done => Mood::Done,
            Status::Working => Mood::Working,
            Status::Waiting => Mood::Waiting,
            Status::Blocked => Mood::Blocked,
        })
        .max_by_key(|&m| m as u8)
}

/// Milliseconds on the pet's animation clock.
pub fn clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The cells at the right edge of `area`, directly above its status line.
pub fn rect(area: Rect) -> Option<Rect> {
    (area.width >= COLS && area.height > ROWS)
        .then(|| Rect::new(area.right() - COLS, area.bottom() - 1 - ROWS, COLS, ROWS))
}

const FRAME: Rgb = (0x5b, 0x6f, 0x9e);
const GRID: Rgb = (0x3b, 0x4f, 0x80);
const CELL_A: Rgb = (0x22, 0x40, 0x7f);
const CELL_B: Rgb = (0x2c, 0x4f, 0x98);
const CELL_LIT: Rgb = (0x3a, 0x62, 0xb4);
const CELL_HI: Rgb = (0x8f, 0xb2, 0xff);
const STRUT: Rgb = (0x7c, 0x82, 0x8c);
const BODY: Rgb = (0xc4, 0xca, 0xd5);
const BODY_HI: Rgb = (0xee, 0xf1, 0xf6);
const BODY_LO: Rgb = (0x85, 0x8c, 0x9b);
const BODY_DK: Rgb = (0x5f, 0x66, 0x74);
const SCREEN: Rgb = (0x0c, 0x14, 0x20);
const SCAN: Rgb = (0x10, 0x1b, 0x2a);
const SCREEN_ALARM: Rgb = (0x2a, 0x0c, 0x12);
const SCAN_ALARM: Rgb = (0x33, 0x10, 0x1a);
const EYE: Rgb = (0x6e, 0xe7, 0xf2);
const EYE_HI: Rgb = (0xd9, 0xfb, 0xff);
const EYE_DIM: Rgb = (0x2f, 0x7b, 0x85);
const EYE_ALARM: Rgb = (0xff, 0x6b, 0x70);
const BLUSH: Rgb = (0xff, 0x8f, 0xb1);
const OFF: Rgb = (0x3a, 0x3f, 0x48);
const RED: Rgb = (0xff, 0x4d, 0x55);
const AMBER: Rgb = (0xff, 0xb3, 0x47);
const GREEN: Rgb = (0x5e, 0xe0, 0x8a);
const ORANGE: Rgb = (0xff, 0x8a, 0x3d);
const YELLOW: Rgb = (0xff, 0xe0, 0x66);
const WHITE: Rgb = (0xff, 0xff, 0xff);
const ZZZ: Rgb = (0x9f, 0xb0, 0xcc);

struct Canvas {
    px: Vec<Option<Rgb>>,
    dx: i32,
    dy: i32,
}

impl Canvas {
    fn set(&mut self, x: i32, y: i32, c: Rgb) {
        let (x, y) = (x + self.dx, y + self.dy);
        if (0..WIDTH as i32).contains(&x) && (0..HEIGHT as i32).contains(&y) {
            self.px[y as usize * WIDTH + x as usize] = Some(c);
        }
    }

    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: Rgb) {
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.set(x, y, c);
            }
        }
    }
}

fn antenna(mood: Mood, t: u64) -> Rgb {
    let tf = t as f64;
    match mood {
        Mood::Idle if (tf / 1500.0).sin() > 0.4 => EYE_DIM,
        Mood::Working if (t / 500) % 2 == 1 => GREEN,
        Mood::Waiting if (tf / 380.0).sin() > -0.2 => AMBER,
        Mood::Blocked if (t / 120) % 2 == 1 => RED,
        Mood::Done => GREEN,
        _ => OFF,
    }
}

/// The sprite at `t` milliseconds into its animation, row-major, `None`
/// where nothing is drawn.
pub fn sprite(mood: Mood, t: u64) -> Vec<Option<Rgb>> {
    let mut c = Canvas {
        px: vec![None; WIDTH * HEIGHT],
        dx: 0,
        dy: 0,
    };
    match mood {
        Mood::Idle if (t as f64 / 2400.0 * std::f64::consts::TAU).sin() > 0.0 => c.dy = 1,
        Mood::Waiting if t % 4000 < 260 => c.dy = -1,
        Mood::Done if t % 1400 < 300 => c.dy = -1,
        Mood::Blocked if t % 1600 < 700 => c.dx = if (t / 90) % 2 == 1 { 1 } else { -1 },
        _ => {}
    }
    let wing = match mood {
        Mood::Blocked if (t / 200) % 2 == 1 => -1,
        Mood::Done => -1,
        _ => 0,
    };
    let alarm = mood == Mood::Blocked;

    // Two solar wings with a cell grid and a passing glint.
    let glint = match mood {
        Mood::Working => (t as f64 / 25.0) % 50.0 - 8.0,
        Mood::Idle => (t % 7000) as f64 / 45.0 - 8.0,
        _ => -99.0,
    };
    for (x0, mirror) in [(2, false), (50, true)] {
        for rx in 0..20 {
            for ry in 0..18 {
                let edge = rx == 0 || rx == 19 || ry == 0 || ry == 17;
                let gx = if mirror { 19 - rx } else { rx };
                let line = gx % 5 == 0 || ry % 6 == 0;
                let mut color = if edge {
                    FRAME
                } else if line {
                    GRID
                } else if gx % 5 == 1 && ry % 6 == 1 {
                    CELL_LIT
                } else if (gx / 5 + ry / 6) % 2 == 1 {
                    CELL_A
                } else {
                    CELL_B
                };
                if !edge && (f64::from(gx + ry) - glint).abs() < 1.6 {
                    color = if line { CELL_LIT } else { CELL_HI };
                }
                c.set(x0 + rx, 9 + ry + wing, color);
            }
        }
    }
    c.rect(22, 17, 25, 18, STRUT);
    c.rect(46, 17, 49, 18, STRUT);
    c.rect(24, 16, 25, 19, BODY_DK);
    c.rect(46, 16, 47, 19, BODY_DK);

    // The bus, with a scanlined screen for a face.
    for x in 26..=45 {
        for y in 8..=27 {
            let color = if y == 27 || x == 45 {
                BODY_LO
            } else if y == 8 || x == 26 {
                BODY_HI
            } else {
                BODY
            };
            c.set(x, y, color);
        }
    }
    for (x, y) in [(27, 9), (44, 9), (27, 26), (44, 26)] {
        c.set(x, y, BODY_LO);
    }
    for x in 29..=42 {
        for y in 11..=24 {
            if (x == 29 || x == 42) && (y == 11 || y == 24) {
                continue;
            }
            let color = match (y % 2 == 1, alarm) {
                (true, true) => SCAN_ALARM,
                (true, false) => SCAN,
                (false, true) => SCREEN_ALARM,
                (false, false) => SCREEN,
            };
            c.set(x, y, color);
        }
    }
    c.rect(35, 3, 36, 7, BODY_LO);
    c.rect(34, 1, 37, 2, antenna(mood, t));
    c.rect(33, 28, 38, 28, BODY_LO);
    c.rect(34, 29, 37, 29, BODY_DK);
    if mood == Mood::Working {
        let f = (t / 70) as i32;
        for i in 0..2 + f % 3 {
            let (x0, x1) = match i {
                0 => (34, 37),
                1 | 2 => (35, 36),
                _ => (35, 35 + f % 2),
            };
            c.rect(x0, 30 + i, x1, 30 + i, if i == 0 { YELLOW } else { ORANGE });
        }
    }

    let eye = if alarm { EYE_ALARM } else { EYE };
    let mouth = if alarm { EYE_ALARM } else { EYE_DIM };
    let open = |c: &mut Canvas, ox: i32, oy: i32, h: i32| {
        for ex in [31, 38] {
            c.rect(ex + ox, 14 + oy, ex + ox + 2, 13 + oy + h, eye);
            c.set(ex + ox, 14 + oy, EYE_HI);
        }
    };
    let shut = |c: &mut Canvas, color: Rgb| {
        c.rect(31, 17, 33, 17, color);
        c.rect(38, 17, 40, 17, color);
    };
    match mood {
        Mood::Idle => {
            shut(&mut c, EYE_DIM);
            c.rect(35, 21, 36, 21, EYE_DIM);
            for k in 0..3 {
                let age = ((t + k * 1300) % 3900) as i32;
                let (x, y) = (39 + age / 330, 5 - age / 800);
                let color = if age < 2600 { ZZZ } else { FRAME };
                c.rect(x, y, x + 2, y, color);
                c.set(x + 1, y + 1, color);
                c.rect(x, y + 2, x + 2, y + 2, color);
            }
        }
        Mood::Working => {
            open(&mut c, [-1, 0, 1, 0][(t / 700 % 4) as usize], 0, 4);
            c.rect(34, 21, 37, 21, mouth);
        }
        Mood::Waiting => {
            if t % 3000 < 140 {
                shut(&mut c, eye);
            } else {
                open(&mut c, 0, -1, 4);
            }
            c.rect(35, 21, 36, 21, mouth);
        }
        Mood::Blocked => {
            open(&mut c, 0, -1, 5);
            c.rect(35, 21, 36, 22, mouth);
            if (t / 300) % 2 == 1 {
                c.rect(30, 12, 31, 12, eye);
                c.rect(40, 12, 41, 12, eye);
            }
        }
        Mood::Done => {
            for cx in [32, 39] {
                for (x, y) in [(0, 14), (-1, 15), (1, 15), (-2, 16), (2, 16)] {
                    c.set(cx + x, y, eye);
                }
            }
            c.set(32, 20, mouth);
            c.set(39, 20, mouth);
            c.rect(33, 21, 38, 21, mouth);
            c.rect(30, 19, 31, 19, BLUSH);
            c.rect(40, 19, 41, 19, BLUSH);
            let spots = [
                (6, 3),
                (18, 5),
                (54, 4),
                (66, 2),
                (8, 31),
                (22, 33),
                (50, 32),
                (64, 31),
            ];
            for (i, (x, y)) in spots.into_iter().enumerate() {
                let p = (t / 140 + i as u64 * 3) % 8;
                if p < 2 {
                    let color = if p == 1 { WHITE } else { YELLOW };
                    for (sx, sy) in [(0, 0), (-1, 0), (1, 0), (0, -1), (0, 1)] {
                        c.set(x + sx, y + sy, color);
                    }
                }
            }
        }
    }
    c.px
}

/// Blends each pixel from `bg` toward its own color, `t` of the way.
pub fn fade(px: &mut [Option<Rgb>], bg: Rgb, t: f32) {
    let mix = |from: u8, to: u8| (f32::from(from) + (f32::from(to) - f32::from(from)) * t) as u8;
    for (r, g, b) in px.iter_mut().flatten() {
        (*r, *g, *b) = (mix(bg.0, *r), mix(bg.1, *g), mix(bg.2, *b));
    }
}

/// The sprite scaled to `width`×`height` pixels on `bg`, row by row. An
/// integer scale keeps the pixel art even when the cells allow one.
fn scaled(px: &[Option<Rgb>], bg: Rgb, width: usize, height: usize) -> Vec<Rgb> {
    let fit = (width as f64 / WIDTH as f64).min(height as f64 / HEIGHT as f64);
    let scale = if fit >= 1.0 { fit.floor() } else { fit };
    let (sw, sh) = (WIDTH as f64 * scale, HEIGHT as f64 * scale);
    let (ox, oy) = ((width as f64 - sw) / 2.0, (height as f64 - sh) / 2.0);
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let (sx, sy) = ((x as f64 - ox) / scale, (y as f64 - oy) / scale);
            let inside = sx >= 0.0 && sy >= 0.0 && (sx as usize) < WIDTH && (sy as usize) < HEIGHT;
            out.push(if inside {
                px[sy as usize * WIDTH + sx as usize].unwrap_or(bg)
            } else {
                bg
            });
        }
    }
    out
}

/// The image id the pet's kitty image is transmitted, placed, and
/// deleted under.
const KITTY_ID: u32 = 7_573_620;

/// The sprite scaled to `width`×`height` pixels on `bg`, as a kitty
/// graphics image placed over `cols`×`rows` cells at the cursor, which
/// stays put.
pub fn kitty(
    px: &[Option<Rgb>],
    bg: Rgb,
    width: usize,
    height: usize,
    cols: u16,
    rows: u16,
) -> String {
    use base64::Engine as _;
    let rgb: Vec<u8> = scaled(px, bg, width, height)
        .into_iter()
        .flat_map(|(r, g, b)| [r, g, b])
        .collect();
    let data = base64::engine::general_purpose::STANDARD.encode(rgb);
    // Payloads go in chunks of at most 4096 bytes.
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let chunk = std::str::from_utf8(chunk).unwrap_or_default();
        if i == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=T,f=24,s={width},v={height},i={KITTY_ID},p=1,c={cols},r={rows},C=1,q=2,m={more};{chunk}\x1b\\"
            );
        } else {
            let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
        }
    }
    out
}

/// Deletes the pet's kitty image, placement and data both.
pub fn kitty_delete() -> String {
    format!("\x1b_Ga=d,d=I,i={KITTY_ID},q=2\x1b\\")
}

/// The sprite scaled to `width`×`height` pixels on `bg`, as a sixel image.
pub fn sixel(px: &[Option<Rgb>], bg: Rgb, width: usize, height: usize) -> String {
    let mut colors: Vec<Rgb> = vec![bg];
    let image: Vec<usize> = scaled(px, bg, width, height)
        .into_iter()
        .map(|color| match colors.iter().position(|&c| c == color) {
            Some(i) => i,
            None => {
                colors.push(color);
                colors.len() - 1
            }
        })
        .collect();

    let mut out = format!("\x1bP9;1q\"1;1;{width};{height}");
    let pct = |v: u8| u32::from(v) * 100 / 255;
    for (i, (r, g, b)) in colors.iter().enumerate() {
        let _ = write!(out, "#{i};2;{};{};{}", pct(*r), pct(*g), pct(*b));
    }
    for band in (0..height).step_by(6) {
        let rows = (height - band).min(6);
        for color in 0..colors.len() {
            let bits: Vec<u8> = (0..width)
                .map(|x| {
                    (0..rows)
                        .filter(|r| image[(band + r) * width + x] == color)
                        .fold(0, |acc, r| acc | 1 << r)
                })
                .collect();
            let Some(last) = bits.iter().rposition(|&b| b != 0) else {
                continue;
            };
            let _ = write!(out, "#{color}");
            let mut x = 0;
            while x <= last {
                let run = bits[x..=last].iter().take_while(|&&b| b == bits[x]).count();
                let ch = char::from(63 + bits[x]);
                if run > 3 {
                    let _ = write!(out, "!{run}{ch}");
                } else {
                    out.extend(std::iter::repeat_n(ch, run));
                }
                x += run;
            }
            out.push('$');
        }
        out.push('-');
    }
    out.push_str("\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_urgent_agent_sets_the_mood() {
        assert_eq!(mood([]), None);
        assert_eq!(mood([Status::Idle, Status::Waiting]), Some(Mood::Waiting));
        assert_eq!(
            mood([Status::Waiting, Status::Working]),
            Some(Mood::Working)
        );
        assert_eq!(
            mood([Status::Working, Status::Waiting, Status::Done]),
            Some(Mood::Done)
        );
        assert_eq!(
            mood([Status::Blocked, Status::Done, Status::Idle]),
            Some(Mood::Blocked)
        );
    }

    #[test]
    fn the_pet_sits_at_the_right_edge_above_the_status_line() {
        assert_eq!(
            rect(Rect::new(20, 0, 60, 24)),
            Some(Rect::new(72, 21, 8, 2))
        );
        assert_eq!(rect(Rect::new(0, 0, 7, 24)), None);
        assert_eq!(rect(Rect::new(0, 0, 80, 2)), None);
    }

    #[test]
    fn every_mood_draws_the_bus() {
        for m in [
            Mood::Idle,
            Mood::Done,
            Mood::Working,
            Mood::Waiting,
            Mood::Blocked,
        ] {
            let px = sprite(m, 1234);
            assert_eq!(px.len(), WIDTH * HEIGHT);
            assert!(px[20 * WIDTH + 27].is_some(), "{m:?}");
        }
    }

    #[test]
    fn fade_blends_from_the_background_and_leaves_gaps_alone() {
        let mut px = vec![Some((200, 100, 0)), None];
        fade(&mut px, (0, 0, 100), 0.5);
        assert_eq!(px, vec![Some((100, 50, 50)), None]);
        let mut full = vec![Some((200, 100, 0))];
        fade(&mut full, (0, 0, 100), 1.0);
        assert_eq!(full, vec![Some((200, 100, 0))]);
    }

    #[test]
    fn sixel_scales_by_whole_pixels_and_fills_the_background() {
        let mut px = vec![None; WIDTH * HEIGHT];
        px[0] = Some((255, 0, 0));
        let out = sixel(&px, (0, 0, 0), 2 * WIDTH, 2 * HEIGHT);
        assert!(out.starts_with("\x1bP9;1q\"1;1;144;72#0;2;0;0;0#1;2;100;0;0"));
        assert!(out.ends_with("-\x1b\\"));
        // The red pixel doubles to a 2×2 block: two columns of the low two
        // bits in the first band.
        assert!(out.contains("#1BB$"));
        assert_eq!(out.matches('-').count(), 12);
    }

    #[test]
    fn kitty_sends_rgb_in_chunks_placed_over_the_cells() {
        let px = vec![None; WIDTH * HEIGHT];
        let out = kitty(&px, (0, 0, 0), WIDTH, HEIGHT, COLS, ROWS);
        let first = out.split("\x1b\\").next().unwrap();
        assert!(first.starts_with("\x1b_Ga=T,f=24,s=72,v=36,"));
        assert!(first.contains(",c=8,r=2,C=1,q=2,m=1;"));
        // 72×36×3 bytes is 10368 base64 characters: three chunks.
        assert_eq!(out.matches("\x1b_G").count(), 3);
        assert_eq!(out.matches("m=1;").count(), 2);
        assert!(out.contains("\x1b_Gm=0;"));
    }
}

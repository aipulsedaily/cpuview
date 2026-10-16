//! Colour helpers. Colours are packed `0xRRGGBB` in a u32.

use std::sync::OnceLock;

pub type Rgb = u32;

#[inline]
pub fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}
#[inline]
pub fn ch(c: Rgb) -> [u8; 3] {
    [(c >> 16) as u8, (c >> 8) as u8, c as u8]
}
#[inline]
pub fn chf(c: Rgb) -> [f32; 3] {
    let [r, g, b] = ch(c);
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0]
}
#[inline]
pub fn from_f(c: [f32; 3]) -> Rgb {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    rgb(q(c[0]), q(c[1]), q(c[2]))
}

pub fn hex(s: &str) -> Rgb {
    u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0) & 0xFF_FFFF
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let (a, b) = (ch(a), ch(b));
    let m =
        |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t + 0.5).clamp(0.0, 255.0) as u8;
    rgb(m(0), m(1), m(2))
}

fn lin_table() -> &'static [f32; 256] {
    static T: OnceLock<[f32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *v = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
        }
        t
    })
}

/// `mix` in linear light: an anti-aliased edge covering `t` of a cell emits `t` of
/// the colour's light, so a bar end moves at an even perceived speed across a cell.
pub fn mix_lin(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let l = lin_table();
    let (a, b) = (ch(a), ch(b));
    let m = |i: usize| {
        let v = l[a[i] as usize] + (l[b[i] as usize] - l[a[i] as usize]) * t;
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8
    };
    rgb(m(0), m(1), m(2))
}

/// WCAG relative luminance.
pub fn lum(c: Rgb) -> f32 {
    let t = lin_table();
    let [r, g, b] = ch(c);
    0.2126 * t[r as usize] + 0.7152 * t[g as usize] + 0.0722 * t[b as usize]
}

pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (lum(a), lum(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// A gradient sampled into 65 steps (like the Python version's `ramp`).
#[derive(Clone)]
pub struct Ramp(pub [Rgb; 65]);

impl Ramp {
    /// Stops at given positions (0..1), linear interpolation in sRGB.
    pub fn new(stops: &[(f32, Rgb)]) -> Ramp {
        let mut t = [0; 65];
        for (q, v) in t.iter_mut().enumerate() {
            *v = ramp_at(stops, q as f32 / 64.0);
        }
        Ramp(t)
    }
    pub fn even(cols: &[Rgb]) -> Ramp {
        let n = cols.len().max(2) - 1;
        let stops: Vec<(f32, Rgb)> = cols
            .iter()
            .enumerate()
            .map(|(i, &c)| (i as f32 / n as f32, c))
            .collect();
        Ramp::new(&stops)
    }
    #[inline]
    pub fn at(&self, x: f32) -> Rgb {
        let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
        self.0[(x * 64.0 + 0.5) as usize]
    }
    /// A darker copy blended towards `base` (idle colours).
    pub fn shade(&self, base: Rgb, k: f32) -> Ramp {
        let mut t = self.0;
        for v in t.iter_mut() {
            *v = mix(base, *v, k);
        }
        Ramp(t)
    }
}

/// Exact (unsampled) evaluation of a stop list, f32 0..1 colours, as in reference.py `ramp()`.
pub fn ramp_at(stops: &[(f32, Rgb)], t: f32) -> Rgb {
    from_f(ramp_f(stops, t))
}

pub fn ramp_f(stops: &[(f32, Rgb)], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    if stops.is_empty() {
        return [0.0; 3];
    }
    if t <= stops[0].0 {
        return chf(stops[0].1);
    }
    for w in stops.windows(2) {
        let ((p0, c0), (p1, c1)) = (w[0], w[1]);
        if t <= p1 {
            let k = if p1 > p0 { (t - p0) / (p1 - p0) } else { 0.0 };
            let (a, b) = (chf(c0), chf(c1));
            return [
                a[0] + (b[0] - a[0]) * k,
                a[1] + (b[1] - a[1]) * k,
                a[2] + (b[2] - a[2]) * k,
            ];
        }
    }
    chf(stops[stops.len() - 1].1)
}

// ── HSV (palette from image) ──────────────────────────────────────────────────

pub fn rgb_to_hsv(c: [f32; 3]) -> [f32; 3] {
    let (r, g, b) = (c[0], c[1], c[2]);
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let v = mx;
    if mx == mn {
        return [0.0, 0.0, v];
    }
    let s = (mx - mn) / mx;
    let rc = (mx - r) / (mx - mn);
    let gc = (mx - g) / (mx - mn);
    let bc = (mx - b) / (mx - mn);
    let h = if r == mx {
        bc - gc
    } else if g == mx {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    [(h / 6.0).rem_euclid(1.0), s, v]
}

pub fn hsv_to_rgb(c: [f32; 3]) -> [f32; 3] {
    let (h, s, v) = (c[0], c[1], c[2]);
    if s == 0.0 {
        return [v, v, v];
    }
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i32).rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

// ── xterm-256 ─────────────────────────────────────────────────────────────────

const LV: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn cube_idx(v: i32) -> usize {
    if v < 48 {
        0
    } else if v < 115 {
        1
    } else {
        ((v - 35) / 40).clamp(1, 5) as usize
    }
}

/// Nearest xterm-256 colour (6×6×6 cube or grey ramp). No dithering: dark UI
/// gradients sit between very different palette entries, and dithering them turns
/// calm backgrounds into noise; flat bands look better.
pub fn to_256(c: Rgb, x: usize, y: usize) -> u8 {
    let _ = (x, y);
    let d = 0;
    let [r, g, b] = ch(c);
    let (r, g, b) = (
        (r as i32 + d).clamp(0, 255),
        (g as i32 + d).clamp(0, 255),
        (b as i32 + d).clamp(0, 255),
    );
    let (ri, gi, bi) = (cube_idx(r), cube_idx(g), cube_idx(b));
    let cube = (LV[ri] as i32, LV[gi] as i32, LV[bi] as i32);
    let dc = (cube.0 - r).pow(2) + (cube.1 - g).pow(2) + (cube.2 - b).pow(2);
    let avg = (r + g + b) / 3;
    let gi2 = if avg > 238 {
        23
    } else {
        ((avg - 3).max(0) / 10).min(23)
    };
    let gv = 8 + 10 * gi2;
    let dg = (gv - r).pow(2) + (gv - g).pow(2) + (gv - b).pow(2);
    if dg < dc {
        (232 + gi2) as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contrast_and_ramp() {
        assert!((contrast(0xffffff, 0x000000) - 21.0).abs() < 0.01);
        let r = Ramp::even(&[0x000000, 0xffffff]);
        assert_eq!(r.at(0.0), 0);
        assert_eq!(r.at(1.0), 0xffffff);
        assert_eq!(to_256(0xff0000, 1, 1), 196);
        let h = rgb_to_hsv([0.2, 0.4, 0.8]);
        let b = hsv_to_rgb(h);
        assert!((b[0] - 0.2).abs() < 1e-5 && (b[2] - 0.8).abs() < 1e-5);
    }
}

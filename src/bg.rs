//! Procedural backgrounds: a port of themes/reference.py (layers, hash, noise).
//! The integer hash is bit-exact with the reference; float maths uses f64 like Python.

use crate::color::{chf, hex, ramp_f, Rgb};
use crate::theme::{stops, P};
use std::f64::consts::PI;

pub fn hash_u32(ix: i64, iy: i64, seed: i64) -> u32 {
    let h = (ix as u32)
        .wrapping_mul(374761393)
        .wrapping_add((iy as u32).wrapping_mul(668265263))
        .wrapping_add((seed as u32).wrapping_mul(2246822519));
    let h = (h ^ (h >> 13)).wrapping_mul(1274126177);
    h ^ (h >> 16)
}

#[inline]
pub fn hash01(ix: i64, iy: i64, seed: i64) -> f64 {
    hash_u32(ix, iy, seed) as f64 / 4294967295.0
}

#[inline]
fn fade(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

pub fn value_noise(x: f64, y: f64, seed: i64) -> f64 {
    let (ix, iy) = (x.floor(), y.floor());
    let (fx, fy) = (fade(x - ix), fade(y - iy));
    let (ix, iy) = (ix as i64, iy as i64);
    let a = hash01(ix, iy, seed);
    let b = hash01(ix + 1, iy, seed);
    let c = hash01(ix, iy + 1, seed);
    let d = hash01(ix + 1, iy + 1, seed);
    let top = a + (b - a) * fx;
    let bot = c + (d - c) * fx;
    top + (bot - top) * fy
}

pub fn fbm(x: f64, y: f64, seed: i64, octaves: i64, lac: f64, gain: f64, ridged: bool) -> f64 {
    let (mut amp, mut freq, mut total, mut norm) = (1.0, 1.0, 0.0, 0.0);
    for k in 0..octaves {
        let mut n = value_noise(x * freq, y * freq, seed + k);
        if ridged {
            n = 1.0 - (2.0 * n - 1.0).abs();
        }
        total += amp * n;
        norm += amp;
        amp *= gain;
        freq *= lac;
    }
    if norm > 0.0 {
        total / norm
    } else {
        0.0
    }
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

fn contrast_curve(v: f64, thr: f64, pw: f64) -> f64 {
    let v = if thr < 1.0 {
        clamp01((v - thr) / (1.0 - thr))
    } else {
        0.0
    };
    v.powf(pw)
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub hires: bool,
    pub px: Vec<[f32; 3]>,
    /// Cell grid for glyph layers.
    pub gw: usize,
    pub gh: usize,
    /// Per cell: glyph char ('\0' = none) and colour.
    pub glyph: Vec<(char, Rgb)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Over,
    Add,
    Screen,
    Multiply,
}

fn mode(p: &P, d: Mode) -> Mode {
    match p.s("blend") {
        Some("add") => Mode::Add,
        Some("screen") => Mode::Screen,
        Some("multiply") => Mode::Multiply,
        Some("over") => Mode::Over,
        _ => d,
    }
}

#[inline]
fn blend(d: [f32; 3], s: [f32; 3], a: f64, m: Mode) -> [f32; 3] {
    if a <= 0.0 {
        return d;
    }
    let a = a as f32;
    let mut o = d;
    for k in 0..3 {
        o[k] = match m {
            Mode::Add => (d[k] + s[k] * a).min(1.0),
            Mode::Screen => d[k] + (1.0 - d[k]) * s[k] * a,
            Mode::Multiply => d[k] * (1.0 - a + s[k] * a),
            Mode::Over => d[k] + (s[k] - d[k]) * a,
        }
    }
    o
}

impl Canvas {
    fn coords(&self) -> impl Iterator<Item = (usize, usize, f64, f64, f64)> {
        let (w, h) = (self.w, self.h);
        (0..h).flat_map(move |y| {
            let v = (y as f64 + 0.5) / h as f64;
            (0..w).map(move |x| {
                (
                    x,
                    y,
                    (x as f64 + 0.5) / w as f64,
                    v,
                    (x as f64 + 0.5) / h as f64,
                )
            })
        })
    }
}

fn ramp3(st: &[(f32, Rgb)], t: f64) -> [f32; 3] {
    ramp_f(st, t as f32)
}

fn layer_gradient(cv: &mut Canvas, p: &P) {
    let st = stops(p.arr("stops"));
    let a = p.n("angle", 0.0).to_radians();
    let (dx, dy) = (a.sin(), a.cos());
    let corners = [0.0, dy, dx, dx + dy];
    let lo = corners.iter().cloned().fold(f64::MAX, f64::min);
    let hi = corners.iter().cloned().fold(f64::MIN, f64::max);
    let (m, op) = (mode(p, Mode::Over), p.n("opacity", 1.0));
    let w = cv.w;
    for (x, y, u, v, _) in cv.coords().collect::<Vec<_>>() {
        let t = (u * dx + v * dy - lo) / (hi - lo);
        let i = y * w + x;
        cv.px[i] = blend(cv.px[i], ramp3(&st, t), op, m);
    }
}

fn layer_radial(cv: &mut Canvas, p: &P) {
    let st = stops(p.arr("stops"));
    let (cu, cvv) = p.pair("center", (0.5, 0.5));
    let r = p.n("radius", 0.5);
    let (m, op) = (mode(p, Mode::Screen), p.n("opacity", 1.0));
    let fall = p.n("falloff", 1.5);
    let cx = cu * cv.w as f64 / cv.h as f64;
    let w = cv.w;
    for (x, y, _, v, s) in cv.coords().collect::<Vec<_>>() {
        let d = (s - cx).hypot(v - cvv) / r;
        if d < 1.0 {
            let i = y * w + x;
            cv.px[i] = blend(cv.px[i], ramp3(&st, d), op * (1.0 - d).powf(fall), m);
        }
    }
}

fn band_mask(p: &P, u: f64, v: f64) -> f64 {
    match p.get("band") {
        Some(b) => {
            let d = (v - (b.n("center", 0.5) + b.n("slope", 0.0) * (u - 0.5))) / b.n("width", 0.2);
            (-d * d).exp()
        }
        None => 1.0,
    }
}

fn layer_noise(cv: &mut Canvas, p: &P) {
    let seed = p.n("seed", 1.0) as i64;
    let sc = p.n("scale", 3.0);
    let oc = p.n("octaves", 5.0) as i64;
    let lac = p.n("lacunarity", 2.0);
    let gain = p.n("gain", 0.5);
    let ridged = p.b("ridged");
    let warp = p.n("warp", 0.0);
    let (sx, sy) = p.pair("stretch", (1.0, 1.0));
    let st = stops(p.arr("colors"));
    let (m, op) = (mode(p, Mode::Screen), p.n("opacity", 1.0));
    let (thr, pw) = (p.n("threshold", 0.0), p.n("power", 1.0));
    let w = cv.w;
    for (x, y, u, v, s) in cv.coords().collect::<Vec<_>>() {
        let (mut nx, mut ny) = (s * sc * sx, v * sc * sy);
        if warp != 0.0 {
            let wx = fbm(nx + 5.2, ny + 1.3, seed + 101, 3, 2.0, 0.5, false);
            let wy = fbm(nx + 1.7, ny + 9.2, seed + 202, 3, 2.0, 0.5, false);
            nx += warp * (wx - 0.5) * 2.0;
            ny += warp * (wy - 0.5) * 2.0;
        }
        let n = fbm(nx, ny, seed, oc, lac, gain, ridged);
        let mm = contrast_curve(n, thr, pw) * band_mask(p, u, v);
        if mm > 0.0 {
            let i = y * w + x;
            cv.px[i] = blend(cv.px[i], ramp3(&st, mm), op * mm, m);
        }
    }
}

fn layer_stars(cv: &mut Canvas, p: &P) {
    let seed = p.n("seed", 7.0) as i64;
    let dens = p.n("density", 0.004);
    let grad = p.n("density_gradient", 0.0);
    let min_b = p.n("min_brightness", 0.25);
    let mut cols: Vec<Rgb> = p.arr("colors").iter().map(|c| hex(c.as_s())).collect();
    if cols.is_empty() {
        cols.push(0xffffff);
    }
    let n = cols.len();
    if cv.hires {
        let w = cv.w;
        for (x, y, _, v, _) in cv.coords().collect::<Vec<_>>() {
            let d = dens * (1.0 + grad * (v - 0.5) * 2.0).max(0.0);
            if hash01(x as i64, y as i64, seed) < d {
                let b = min_b + (1.0 - min_b) * hash01(x as i64, y as i64, seed + 1).powi(3);
                let c = cols[(hash01(x as i64, y as i64, seed + 2) * n as f64) as usize % n];
                let i = y * w + x;
                cv.px[i] = blend(cv.px[i], chf(c), b, Mode::Add);
            }
        }
    }
    let gd = p.n("glyph_density", 0.0);
    let gl: Vec<char> = {
        let a = p.arr("glyphs");
        if a.is_empty() {
            vec!['·', '·', '∙', '•']
        } else {
            a.iter().filter_map(|g| g.as_s().chars().next()).collect()
        }
    };
    if gd > 0.0 {
        let rows = cv.gh;
        for cy in 0..rows {
            let v = (cy as f64 + 0.5) / rows as f64;
            let d = gd * (1.0 + grad * (v - 0.5) * 2.0).max(0.0);
            for cx in 0..cv.gw {
                let (x, y) = (cx as i64, cy as i64);
                if hash01(x, y, seed + 3) < d {
                    let g =
                        gl[(hash01(x, y, seed + 4).powi(2) * gl.len() as f64) as usize % gl.len()];
                    let b = min_b + (1.0 - min_b) * hash01(x, y, seed + 5).powi(2);
                    let c = chf(cols[(hash01(x, y, seed + 6) * n as f64) as usize % n]);
                    let c =
                        crate::color::from_f([c[0] * b as f32, c[1] * b as f32, c[2] * b as f32]);
                    cv.glyph[cy * cv.gw + cx] = (g, c);
                }
            }
        }
    }
}

fn layer_aurora(cv: &mut Canvas, p: &P) {
    let seed = p.n("seed", 3.0) as i64;
    let op = p.n("opacity", 1.0);
    let w = cv.w;
    for (i, b) in p.arr("bands").iter().enumerate() {
        let st = stops(&[]);
        let _ = st;
        let bc = hex(b.s("bottom_color").unwrap_or("#3dff9e"));
        let tc = hex(b.s("top_color").unwrap_or("#7b4dff"));
        let st = [(0.0f32, bc), (1.0f32, tc)];
        let y0 = b.n("y", 0.5);
        let amp = b.n("amplitude", 0.06);
        let waves = b.n("waves", 1.5);
        let phase = b.n("phase", 0.0);
        let h = b.n("height", 0.18);
        let streaks = b.n("streaks", 90.0);
        let inten = b.n("intensity", 1.0);
        let sd = seed + 31 * i as i64;
        let line: Vec<f64> = (0..w)
            .map(|x| {
                let u = (x as f64 + 0.5) / w as f64;
                y0 + amp * (2.0 * PI * (u * waves + phase)).sin()
                    + amp * (fbm(u * 3.0, 0.0, sd, 3, 2.0, 0.5, false) - 0.5)
            })
            .collect();
        let rays: Vec<f64> = (0..w)
            .map(|x| 0.55 + 0.45 * value_noise((x as f64 + 0.5) / w as f64 * streaks, 0.5, sd + 9))
            .collect();
        for (x, y, _, v, _) in cv.coords().collect::<Vec<_>>() {
            let d = line[x] - v;
            let k = if d > 0.0 {
                rays[x] * (-d / h).exp()
            } else {
                (d / (h * 0.15)).exp()
            };
            if k > 0.004 {
                let j = y * w + x;
                cv.px[j] = blend(
                    cv.px[j],
                    ramp3(&st, clamp01(d / h)),
                    op * k * inten,
                    Mode::Add,
                );
            }
        }
    }
}

fn layer_sun(cv: &mut Canvas, p: &P) {
    let (cu, cvv) = p.pair("center", (0.5, 0.5));
    let r = p.n("radius", 0.2);
    let st = stops(p.arr("stops"));
    let cx = cu * cv.w as f64 / cv.h as f64;
    let stripes = p.n("stripes", 6.0);
    let glow = chf(p
        .s("glow")
        .map(hex)
        .unwrap_or_else(|| st.last().map(|s| s.1).unwrap_or(0)));
    let clip = p.n("clip_below", 2.0);
    let gs = p.n("glow_size", 1.2);
    let go = p.n("glow_opacity", 0.35);
    let w = cv.w;
    for (x, y, _, v, s) in cv.coords().collect::<Vec<_>>() {
        if v > clip {
            continue;
        }
        let d = (s - cx).hypot(v - cvv);
        let t = (v - (cvv - r)) / (2.0 * r);
        let i = y * w + x;
        if d < r {
            let mut gap = false;
            if stripes != 0.0 && t > 0.5 {
                let k = (t - 0.5) * 2.0;
                let period = 1.0 / stripes;
                let ph = k.rem_euclid(period) / period;
                gap = ph < 0.15 + 0.55 * k;
            }
            if !gap {
                cv.px[i] = ramp3(&st, t);
                continue;
            }
        }
        let g = if d >= r {
            (1.0 - (d - r) / (r * gs)).max(0.0)
        } else {
            0.35
        };
        if g > 0.0 {
            cv.px[i] = blend(cv.px[i], glow, go * g * g, Mode::Add);
        }
    }
}

fn layer_grid(cv: &mut Canvas, p: &P) {
    let hz = p.n("horizon", 0.6);
    let col = chf(hex(p.s("color").unwrap_or("#ff3ec8")));
    let dens = p.n("density", 1.0);
    let ncol = p.n("columns", 10.0);
    let lw = p.n("line", 0.06);
    let zs = p.n("depth_scale", 0.12);
    let fd = p.n("fade_distance", 0.25);
    let op = p.n("opacity", 1.0);
    let w = cv.w;
    for (x, y, u, v, _) in cv.coords().collect::<Vec<_>>() {
        if v <= hz {
            continue;
        }
        let dz = v - hz;
        let z = zs / dz;
        let fz = (z * dens).rem_euclid(1.0);
        let fx = ((u - 0.5) / dz * ncol * 0.1).rem_euclid(1.0);
        let hl = (1.0 - fz.min(1.0 - fz) / lw).max(0.0);
        let vl = (1.0 - fx.min(1.0 - fx) / (lw * 1.6)).max(0.0);
        let k = hl.max(vl) * clamp01(dz / fd);
        if k > 0.0 {
            let i = y * w + x;
            cv.px[i] = blend(cv.px[i], col, op * k, Mode::Add);
        }
    }
}

fn layer_silhouette(cv: &mut Canvas, p: &P) {
    let trees = p.s("kind") == Some("trees");
    let seed = p.n("seed", 5.0) as i64;
    let base = p.n("base", 0.85);
    let hgt = p.n("height", 0.15);
    let rough = p.n("roughness", 4.0);
    let col = chf(hex(p.s("color").unwrap_or("#000000")));
    let op = p.n("opacity", 1.0);
    let rim = p.s("rim").map(|s| chf(hex(s)));
    let tw = p.n("tree_width", 6.0);
    let th = p.n("tree_height", 0.12);
    let fill = p.n("tree_fill", 0.85);
    let w = cv.w;
    let ridge: Vec<f64> = (0..w)
        .map(|x| {
            let u = (x as f64 + 0.5) / w as f64;
            let mut r = base - hgt * fbm(u * rough, 0.0, seed, 5, 2.0, 0.5, false);
            if trees {
                let twi = tw as i64;
                r = base - hgt * 0.3 * fbm(u * rough, 0.0, seed, 3, 2.0, 0.5, false);
                let xb = (x as i64).div_euclid(twi.max(1));
                for b in [xb - 1, xb, xb + 1] {
                    if hash01(b, 0, seed + 11) < fill {
                        let centre = (b as f64 + hash01(b, 1, seed + 11)) * tw;
                        let h = th * (0.5 + hash01(b, 2, seed + 11));
                        let half = tw * 0.75 * (0.7 + 0.6 * hash01(b, 3, seed + 11));
                        let dx = (x as f64 + 0.5 - centre).abs() / half;
                        if dx < 1.0 {
                            let jag = 0.08 * hash01(x as i64, 7, seed + 13);
                            r = r.min(base - h * (1.0 - dx) + h * jag);
                        }
                    }
                }
            }
            r
        })
        .collect();
    let hh = cv.h as f64;
    for (x, y, _, v, _) in cv.coords().collect::<Vec<_>>() {
        if v >= ridge[x] {
            let mut c = col;
            if let Some(rm) = rim {
                if v - ridge[x] < 1.0 / hh {
                    c = blend(col, rm, 0.6, Mode::Add);
                }
            }
            let i = y * w + x;
            cv.px[i] = blend(cv.px[i], c, op, Mode::Over);
        }
    }
}

fn layer_rays(cv: &mut Canvas, p: &P) {
    let (cu, cvv) = p.pair("center", (0.5, -0.2));
    let cx = cu * cv.w as f64 / cv.h as f64;
    let col = chf(hex(p.s("color").unwrap_or("#9fe8ff")));
    let cnt = p.n("count", 14.0);
    let sharp = p.n("sharpness", 3.0);
    let reach = p.n("reach", 0.7);
    let seed = p.n("seed", 4.0) as i64;
    let op = p.n("opacity", 0.25);
    let w = cv.w;
    for (x, y, _, v, s) in cv.coords().collect::<Vec<_>>() {
        let ang = (s - cx).atan2(v - cvv);
        let dist = (s - cx).hypot(v - cvv);
        let k = value_noise(ang * cnt, 0.0, seed).powf(sharp) * (-dist / reach).exp();
        let i = y * w + x;
        cv.px[i] = blend(cv.px[i], col, op * k, Mode::Add);
    }
}

fn layer_vignette(cv: &mut Canvas, p: &P) {
    let st = p.n("strength", 0.3);
    let w = cv.w;
    for (x, y, u, v, _) in cv.coords().collect::<Vec<_>>() {
        let r2 = ((u - 0.5) * 2.0).powi(2) + ((v - 0.5) * 2.0).powi(2);
        let k = (1.0 - st * r2 / 2.0).max(0.0) as f32;
        let i = y * w + x;
        let c = cv.px[i];
        cv.px[i] = [c[0] * k, c[1] * k, c[2] * k];
    }
}

fn layer_dotgrid(cv: &mut Canvas, p: &P) {
    let (ec, er) = p.pair("every", (4.0, 2.0));
    let (ec, er) = ((ec as usize).max(1), (er as usize).max(1));
    let g = p.s("glyph").and_then(|s| s.chars().next()).unwrap_or('·');
    let c = hex(p.s("color").unwrap_or("#262626"));
    for cy in 0..cv.gh {
        for cx in 0..cv.gw {
            if cx % ec == ec / 2 && cy % er == 0 {
                cv.glyph[cy * cv.gw + cx] = (g, c);
            }
        }
    }
}

/// Render a theme's background: canvas of w × h pixels for a gw × gh cell grid.
pub fn render(
    layers: &[P],
    bg: Rgb,
    w: usize,
    h: usize,
    gw: usize,
    gh: usize,
    hires: bool,
) -> Canvas {
    let mut cv = Canvas {
        w,
        h,
        hires,
        px: vec![chf(bg); w * h],
        gw,
        gh,
        glyph: vec![('\0', 0); gw * gh],
    };
    if w == 0 || h == 0 {
        return cv;
    }
    for l in layers {
        match l.s("type").unwrap_or("") {
            "gradient" => layer_gradient(&mut cv, l),
            "radial" => layer_radial(&mut cv, l),
            "noise" => layer_noise(&mut cv, l),
            "stars" => layer_stars(&mut cv, l),
            "aurora" => layer_aurora(&mut cv, l),
            "sun" => layer_sun(&mut cv, l),
            "grid" => layer_grid(&mut cv, l),
            "silhouette" => layer_silhouette(&mut cv, l),
            "rays" => layer_rays(&mut cv, l),
            "vignette" => layer_vignette(&mut cv, l),
            "dotgrid" => layer_dotgrid(&mut cv, l),
            _ => {}
        }
    }
    cv
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hash_matches_reference() {
        // values computed with themes/reference.py hash_u32
        assert_eq!(hash_u32(0, 0, 0), 0);
        assert_eq!(hash_u32(1, 2, 3), python_hash(1, 2, 3));
        assert_eq!(hash_u32(-5, 7, 101), python_hash(-5, 7, 101));
    }
    // straightforward big-int emulation of the Python reference
    fn python_hash(ix: i64, iy: i64, seed: i64) -> u32 {
        let m = 0xFFFF_FFFFi128;
        let h = ((ix as i128) * 374761393 + (iy as i128) * 668265263 + (seed as i128) * 2246822519)
            .rem_euclid(m + 1);
        let h = ((h ^ (h >> 13)) * 1274126177) & m;
        (h ^ (h >> 16)) as u32
    }
}

#[cfg(test)]
mod dump {
    /// `CPUVIEW_DUMP=dir cargo test -- --ignored dump_canvases`: raw RGB canvases of
    /// every built-in theme at 60x20 for comparison with themes/reference.py.
    #[test]
    #[ignore]
    fn dump_canvases() {
        let dir = std::env::var("CPUVIEW_DUMP").unwrap();
        for id in crate::theme::builtin_ids() {
            let t = crate::theme::load(id).unwrap();
            let cv = super::render(t.layers, t.bg, 60, 40, 60, 20, false);
            let mut out = Vec::new();
            for p in &cv.px {
                out.extend(crate::color::ch(crate::color::from_f(*p)));
            }
            std::fs::write(format!("{dir}/{id}.rgb"), out).unwrap();
            let g: String = cv
                .glyph
                .iter()
                .map(|g| if g.0 == '\0' { '.' } else { g.0 })
                .collect();
            std::fs::write(format!("{dir}/{id}.glyphs"), g).unwrap();
        }
    }
}

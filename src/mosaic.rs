//! Character mosaics: each cell drawn as the best two-colour block glyph for its
//! sub-pixels. Octants (2×4, Unicode 16), sextants (2×3, Unicode 13), quadrants (2×2)
//! or half blocks (1×2, every font). Used wherever a cell can't show real pixels:
//! blank cells in `blocks` mode, and blank cells that change too often for a pixel
//! patch in the pixel modes.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyphs {
    Half,
    Quadrant,
    Sextant,
    Octant,
}

pub const GLYPH_NAMES: [&str; 5] = ["auto", "octant", "sextant", "quadrant", "half"];

impl Glyphs {
    pub fn name(self) -> &'static str {
        match self {
            Glyphs::Half => "half",
            Glyphs::Quadrant => "quadrant",
            Glyphs::Sextant => "sextant",
            Glyphs::Octant => "octant",
        }
    }

    /// `None` = auto
    pub fn parse(s: &str) -> Option<Glyphs> {
        match s {
            "half" => Some(Glyphs::Half),
            "quadrant" => Some(Glyphs::Quadrant),
            "sextant" => Some(Glyphs::Sextant),
            "octant" => Some(Glyphs::Octant),
            _ => None,
        }
    }

    /// sub-pixel grid (columns, rows) of a cell
    pub fn grid(self) -> (usize, usize) {
        match self {
            Glyphs::Half => (1, 2),
            Glyphs::Quadrant => (2, 2),
            Glyphs::Sextant => (2, 3),
            Glyphs::Octant => (2, 4),
        }
    }

    /// The glyph whose ink covers the sub-pixels set in `p` (bit k = sub-pixel k,
    /// row-major). `None` if this set can't draw that pattern.
    pub fn char(self, p: u8) -> Option<char> {
        match self {
            Glyphs::Half => Some([' ', '▀', '▄', '█'][p as usize & 3]),
            Glyphs::Quadrant => Some(QUAD[p as usize & 15]),
            Glyphs::Sextant => sextant(p & 63),
            Glyphs::Octant => octant(p),
        }
    }
}

const QUAD: [char; 16] = [
    ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
];

/// Sextant pattern (bits: 1 2 / 3 4 / 5 6) → U+1FB00.. (60 glyphs; the 4 patterns
/// that already exist elsewhere are skipped in the block's order).
fn sextant(p: u8) -> Option<char> {
    match p {
        0 => Some(' '),
        21 => Some('▌'),
        42 => Some('▐'),
        63 => Some('█'),
        _ => {
            let skip = (p > 21) as u32 + (p > 42) as u32;
            char::from_u32(0x1FB00 + p as u32 - 1 - skip)
        }
    }
}

/// Octant patterns that Unicode encodes outside the octant block (U+1CD00..U+1CDE5),
/// with their characters. The single-corner ones (U+1CEA0..) are Unicode 16 additions
/// few terminals draw yet: they are never chosen (see `allowed`).
const OCT_OTHER: [(u8, char); 26] = [
    (0, ' '),
    (1, '\u{1CEA8}'),
    (2, '\u{1CEAB}'),
    (3, '\u{1FB82}'),
    (5, '▘'),
    (10, '▝'),
    (15, '▀'),
    (20, '\u{1FBE6}'),
    (40, '\u{1FBE7}'),
    (63, '\u{1FB85}'),
    (64, '\u{1CEA3}'),
    (80, '▖'),
    (85, '▌'),
    (90, '▞'),
    (95, '▛'),
    (128, '\u{1CEA0}'),
    (160, '▗'),
    (165, '▚'),
    (170, '▐'),
    (175, '▜'),
    (192, '▂'),
    (240, '▄'),
    (245, '▙'),
    (250, '▟'),
    (252, '▆'),
    (255, '█'),
];

fn octant(p: u8) -> Option<char> {
    let mut skip = 0;
    for &(q, c) in &OCT_OTHER {
        if q == p {
            return Some(c);
        }
        if q < p {
            skip += 1;
        }
    }
    char::from_u32(0x1CD00 + p as u32 - skip)
}

/// Patterns a set may use: everything it can draw except the single-corner octants.
fn allowed(g: Glyphs, p: u8) -> bool {
    !(g == Glyphs::Octant && matches!(p, 1 | 2 | 64 | 128))
}

/// One cell of a mosaic: pattern (bits set = `fg`) and the two colours.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mos {
    pub p: u8,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

/// Best two-colour split of `n` sub-pixels (≤ 8) for glyph set `g`: the pattern and
/// the mean colour of each side, minimising the squared error. Nearly uniform cells
/// become a plain background (pattern 0), so flat areas never show glyph seams.
pub fn solve(g: Glyphs, px: &[[f32; 3]]) -> Mos {
    let n = px.len();
    let mut tot = [0f32; 3];
    for p in px {
        for k in 0..3 {
            tot[k] += p[k];
        }
    }
    let q = |c: [f32; 3], m: f32| -> [u8; 3] {
        let f = |v: f32| (v / m + 0.5).clamp(0.0, 255.0) as u8;
        [f(c[0]), f(c[1]), f(c[2])]
    };
    let mean = q(tot, n as f32);
    let flat = Mos {
        p: 0,
        fg: mean,
        bg: mean,
    };
    // uniform enough: no glyph (an error under ~3 levels per channel is invisible)
    let sse0: f32 = px
        .iter()
        .map(|p| {
            (0..3)
                .map(|k| (p[k] - tot[k] / n as f32).powi(2))
                .sum::<f32>()
        })
        .sum();
    if n < 2 || sse0 < 27.0 * n as f32 {
        return flat;
    }
    let sq = |c: [f32; 3]| c[0] * c[0] + c[1] * c[1] + c[2] * c[2];
    let full: u32 = (1u32 << n) - 1;
    // score of a split = |ΣA|²/nA + |ΣB|²/nB (maximal ⇔ least squared error);
    // masks with sub-pixel 0 in A, enumerated in Gray-code order (one toggle per step)
    let mut sa = px[0];
    let mut best = (f32::MIN, 0u32);
    let mut mask = 1u32;
    for i in 0..1u32 << (n - 1) {
        if i > 0 {
            let bit = i.trailing_zeros() + 1;
            let on = mask & (1 << bit) == 0;
            mask ^= 1 << bit;
            let p = px[bit as usize];
            for k in 0..3 {
                sa[k] += if on { p[k] } else { -p[k] };
            }
        }
        if mask == full {
            continue;
        }
        let na = mask.count_ones() as f32;
        let sb = [tot[0] - sa[0], tot[1] - sa[1], tot[2] - sa[2]];
        let s = sq(sa) / na + sq(sb) / (n as f32 - na);
        if s > best.0
            && (allowed(g, mask as u8) && g.char(mask as u8).is_some()
                || allowed(g, (full ^ mask) as u8) && g.char((full ^ mask) as u8).is_some())
        {
            best = (s, mask);
        }
    }
    if best.0 == f32::MIN {
        return flat;
    }
    let m = best.1;
    let (mut a, mut na) = ([0f32; 3], 0f32);
    for (k, p) in px.iter().enumerate() {
        if m >> k & 1 == 1 {
            for c in 0..3 {
                a[c] += p[c];
            }
            na += 1.0;
        }
    }
    let b = [tot[0] - a[0], tot[1] - a[1], tot[2] - a[2]];
    let (ca, cb) = (q(a, na), q(b, n as f32 - na));
    // draw the pattern whose glyph exists (and prefer the smaller ink area)
    let pa = m as u8;
    let pb = (full ^ m) as u8;
    let ok = |p: u8| allowed(g, p) && g.char(p).is_some();
    if ok(pa) && (!ok(pb) || pa.count_ones() <= pb.count_ones()) {
        Mos {
            p: pa,
            fg: ca,
            bg: cb,
        }
    } else {
        Mos {
            p: pb,
            fg: cb,
            bg: ca,
        }
    }
}

/// Mosaic of a canvas of `w·gx × h·gy` RGB pixels (gx, gy = `g.grid()`).
pub fn build(g: Glyphs, rgb: &[u8], w: usize, h: usize) -> Vec<Mos> {
    let (gx, gy) = g.grid();
    let cw = w * gx;
    let mut px = [[0f32; 3]; 8];
    let mut out = Vec::with_capacity(w * h);
    for cy in 0..h {
        for cx in 0..w {
            for sy in 0..gy {
                for sx in 0..gx {
                    let o = ((cy * gy + sy) * cw + cx * gx + sx) * 3;
                    px[sy * gx + sx] = [rgb[o] as f32, rgb[o + 1] as f32, rgb[o + 2] as f32];
                }
            }
            out.push(solve(g, &px[..gx * gy]));
        }
    }
    out
}

/// Upper / lower half colours of each cell from the same canvas (bars, text plates).
pub fn halves(g: Glyphs, rgb: &[u8], w: usize, h: usize) -> Vec<[u8; 6]> {
    let (gx, gy) = g.grid();
    let cw = w * gx;
    // weight of sub-row r in the upper half: overlap of [r, r+1)/gy with [0, ½)
    let wt = |r: usize| -> f32 { ((0.5 - r as f32 / gy as f32) * gy as f32).clamp(0.0, 1.0) };
    let mut out = Vec::with_capacity(w * h);
    for cy in 0..h {
        for cx in 0..w {
            let mut t = [0f32; 4];
            let mut b = [0f32; 4];
            for sy in 0..gy {
                let wu = wt(sy);
                for sx in 0..gx {
                    let o = ((cy * gy + sy) * cw + cx * gx + sx) * 3;
                    for k in 0..3 {
                        t[k] += rgb[o + k] as f32 * wu;
                        b[k] += rgb[o + k] as f32 * (1.0 - wu);
                    }
                    t[3] += wu;
                    b[3] += 1.0 - wu;
                }
            }
            let f = |v: f32, n: f32| (v / n.max(1e-6) + 0.5).clamp(0.0, 255.0) as u8;
            out.push([
                f(t[0], t[3]),
                f(t[1], t[3]),
                f(t[2], t[3]),
                f(b[0], b[3]),
                f(b[1], b[3]),
                f(b[2], b[3]),
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_points() {
        // first and last octants, and the skipped patterns, from UnicodeData 16.0
        assert_eq!(octant(4), Some('\u{1CD00}')); // BLOCK OCTANT-3
        assert_eq!(octant(6), Some('\u{1CD01}')); // BLOCK OCTANT-23
        assert_eq!(octant(16), Some('\u{1CD09}')); // BLOCK OCTANT-5
        assert_eq!(octant(254), Some('\u{1CDE5}')); // BLOCK OCTANT-2345678
        assert_eq!(octant(15), Some('▀'));
        assert_eq!(octant(165), Some('▚'));
        // every pattern maps to a distinct character
        let mut all: Vec<char> = (0..=255u8).map(|p| octant(p).unwrap()).collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 256);
        assert_eq!(sextant(1), Some('\u{1FB00}')); // BLOCK SEXTANT-1
        assert_eq!(sextant(20), Some('\u{1FB13}')); // BLOCK SEXTANT-35
        assert_eq!(sextant(22), Some('\u{1FB14}')); // BLOCK SEXTANT-235
        assert_eq!(sextant(62), Some('\u{1FB3B}')); // BLOCK SEXTANT-23456
        assert_eq!(Glyphs::Quadrant.char(9), Some('▚'));
    }

    #[test]
    fn solve_finds_the_edge() {
        let (r, b) = ([255.0, 0.0, 0.0], [0.0, 0.0, 255.0]);
        // left column red, right column blue: ▌ in red over blue (or ▐ in blue)
        let px = [r, b, r, b, r, b, r, b];
        let m = solve(Glyphs::Octant, &px);
        let c = Glyphs::Octant.char(m.p).unwrap();
        assert!(
            (c == '▌' && m.fg == [255, 0, 0] && m.bg == [0, 0, 255])
                || (c == '▐' && m.fg == [0, 0, 255] && m.bg == [255, 0, 0]),
            "{c} {m:?}"
        );
        // a diagonal step: only octants can follow it exactly
        let px = [r, r, r, b, r, b, b, b];
        let m = solve(Glyphs::Octant, &px);
        assert!(m.p == 0b0001_0111 || m.p == 0b1110_1000, "{m:?}");
        // flat: no glyph
        let m = solve(Glyphs::Octant, &[[10.0, 20.0, 30.0]; 8]);
        assert_eq!((m.p, m.fg, m.bg), (0, [10, 20, 30], [10, 20, 30]));
        // a lone corner pixel isn't drawn with the rare single-corner glyphs
        let mut px = [b; 8];
        px[0] = r;
        let m = solve(Glyphs::Octant, &px);
        assert!(allowed(Glyphs::Octant, m.p), "{m:?}");
        // half blocks: top / bottom
        let m = solve(Glyphs::Half, &[r, b]);
        assert_eq!(
            Glyphs::Half.char(m.p),
            Some(if m.fg == [255, 0, 0] { '▀' } else { '▄' })
        );
    }

    #[test]
    fn halves_average_rows() {
        // 1 cell of 2×4 octant pixels: top two rows white, bottom two black
        let mut rgb = vec![0u8; 2 * 4 * 3];
        for v in &mut rgb[..12] {
            *v = 255;
        }
        assert_eq!(
            halves(Glyphs::Octant, &rgb, 1, 1)[0],
            [255, 255, 255, 0, 0, 0]
        );
        let m = build(Glyphs::Octant, &rgb, 1, 1)[0];
        assert_eq!(
            Glyphs::Octant.char(m.p),
            Some(if m.fg[0] == 255 { '▀' } else { '▄' })
        );
    }
}

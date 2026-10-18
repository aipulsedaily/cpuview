//! Cell buffer, the glass compositor (themes/README.md "Surfaces and the glass model")
//! and the diffing emitter.
//!
//! Frames are drawn as *logical* cells (character, colour, surface). Only cells whose
//! logical content changed since the last frame are composited and compared with what
//! the terminal shows, so a typical live frame touches a few hundred cells and writes a
//! few hundred bytes in a single write(). Unchanged frames write nothing.

use crate::color::{ch, contrast, keep_256, lighten_to, lum, mix, mix_lin, rgb, to_256, Rgb, MARK_MIN, TEXT_MIN};
use crate::gfx;
use crate::image::Indexed;
use crate::mosaic::{Glyphs, Mos};
use crate::theme::*;

pub const BOLD: u32 = 1 << 24;
/// fg is the `track` surface seen through glass (empty bar segments)
pub const TRACK: u32 = 1 << 25;
/// blank cell in a frequently changing area: never gets a pixel patch (nor a star)
pub const NOPATCH: u32 = 1 << 26;
/// blank fill cell (screen / empty panel area): a background star may be drawn here.
/// Blanks written by widgets never get one, so a star can't be mistaken for data.
pub const STAR_OK: u32 = 1 << 27;
/// (the same bit on a glyph) an area under a graph's curve: its colour as it is over a
/// picture (no 3:1 lift, which would make it as light as the curve or lighter)
pub const FILL: u32 = 1 << 27;
pub const DEFAULT: u32 = 1 << 24;
/// fg bits 28..31 = k (1..15): the colour covers k/16 of the cell and is blended
/// into the track surface in linear light (the anti-aliased edge cell of a bar)
pub const BLEND_SHIFT: u32 = 28;
pub const BLEND: u32 = 15 << BLEND_SHIFT;

#[inline]
fn blend_k(fg: u32) -> f32 {
    ((fg & BLEND) >> BLEND_SHIFT) as f32 / 16.0
}

#[inline]
pub const fn surf(s: u8) -> u32 {
    (s as u32) << 24
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
}

pub const BLANK: Cell = Cell {
    ch: ' ',
    fg: STAR_OK,
    bg: surf(S_SCREEN),
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Out {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    /// a blank cell showing the backdrop (a pixel patch may cover it)
    pub pix: bool,
    /// text whose background had to be darkened to be readable
    pub plate: bool,
}

impl Out {
    #[inline]
    fn key(&self) -> u64 {
        let a = (self.ch as u64) << 32 | self.fg as u64;
        let b = (self.bg as u64) << 1 | self.bold as u64;
        (a ^ b.rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Depth {
    True,
    C256,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Look {
    /// palette colours as-is
    Solid,
    /// surfaces use the terminal's default background
    Transparent,
    /// background through tinted glass, half-block pixels
    Glass,
    /// kitty: picture under the text, surfaces default bg, tints as overlays
    KittyGlass,
}

/// A cell's two half-block pixels (upper, lower) in 6 bytes (a 4K screen's canvas:
/// 260 KB instead of 350).
pub type Px2 = [u8; 6];

#[cfg(test)]
#[inline]
pub fn px2(a: Rgb, b: Rgb) -> Px2 {
    let (a, b) = (ch(a), ch(b));
    [a[0], a[1], a[2], b[0], b[1], b[2]]
}

/// Background rendered for the current size: per cell the two half-block pixels and
/// the best block-glyph mosaic, and for pixel modes a quantised copy at the terminal's
/// pixel size (stored at no more than the picture's own resolution).
pub struct Backdrop {
    pub w: usize,
    pub h: usize,
    /// per cell: [upper half pixel, lower half pixel] (interleaved for locality)
    pub sharp: Vec<Px2>,
    pub soft: Option<Vec<Px2>>,
    /// per cell: two-colour glyph mosaic (empty: half blocks from `sharp`)
    pub mos: Vec<Mos>,
    /// per cell: the picture's average over ±3 cells of the row (text colour choice)
    pub wide: Vec<Rgb>,
    pub glyphs: Glyphs,
    pub glyph: Vec<(char, Rgb)>,
    pub has_glyphs: bool,
    pub hires: Option<Hires>,
}

pub struct Hires {
    pub cw: usize,
    pub ch: usize,
    /// palette images; their size may be smaller than the screen's pixels
    /// (`w·cw × h·ch`): patches then scale them up (nearest)
    pub sharp: Indexed,
    pub soft: Option<Indexed>,
    /// kitty: RGB data to transmit once, dropped after sending
    pub kitty: Option<KittyData>,
}

pub struct KittyData {
    pub rgb: Vec<u8>,
    pub w: usize,
    pub h: usize,
    pub soft: Option<(Vec<u8>, usize, usize)>,
}

impl Backdrop {
    /// Fill `wide` from `sharp`: a ±3-cell running average along each row.
    pub fn widen(&mut self) {
        let (w, h) = (self.w, self.h);
        self.wide = Vec::with_capacity(w * h);
        for y in 0..h {
            let row = &self.sharp[y * w..(y + 1) * w];
            for x in 0..w {
                let (x0, x1) = (x.saturating_sub(3), (x + 4).min(w));
                let mut acc = [0u32; 3];
                for p in &row[x0..x1] {
                    for k in 0..3 {
                        acc[k] += p[k] as u32 + p[k + 3] as u32;
                    }
                }
                let n = 2 * (x1 - x0) as u32;
                self.wide.push(rgb(
                    (acc[0] / n) as u8,
                    (acc[1] / n) as u8,
                    (acc[2] / n) as u8,
                ));
            }
        }
    }

    pub fn empty(w: usize, h: usize) -> Backdrop {
        Backdrop {
            w,
            h,
            sharp: vec![[0; 6]; w * h],
            soft: None,
            mos: Vec::new(),
            wide: Vec::new(),
            glyphs: Glyphs::Half,
            glyph: Vec::new(),
            has_glyphs: false,
            hires: None,
        }
    }
}

/// Surfaces drawn from the soft (blurred) picture: everything but the bare screen.
#[inline]
pub fn soft_surface(s: u8) -> bool {
    matches!(s, S_BAR | S_CHIP | S_PANEL | S_SELECT | S_DIALOG | S_TRACK)
}

#[derive(Clone, Copy)]
struct SurfF {
    tint: [f32; 3],
    tint_rgb: Rgb,
    a: f32,
    ta: f32,
    /// panel-like surface: the tint thickens over bright backdrop pixels
    adapt: bool,
}

pub struct Comp {
    pub look: Look,
    glass: Glass,
    br: f32,
    min_c: f32,
    s: [SurfF; 8],
    solid: [Rgb; 8],
    track_solid: Rgb,
    /// colour of the bars' track drawn into a pixel patch
    pub track_ink: Rgb,
}

#[inline]
fn f3(c: Rgb) -> [f32; 3] {
    let [r, g, b] = ch(c);
    [r as f32, g as f32, b as f32]
}

impl Comp {
    pub fn new(t: &Theme, look: Look) -> Comp {
        let mut s = [SurfF {
            tint: [0.0; 3],
            tint_rgb: 0,
            a: 0.0,
            ta: 0.0,
            adapt: false,
        }; 8];
        let mut solid = [0; 8];
        for i in 1..8 {
            let p = t.surf[i];
            s[i] = SurfF {
                tint: f3(p.tint),
                tint_rgb: p.tint,
                a: p.alpha,
                ta: p.text_alpha,
                // panel glass thickens over bright pixels: the frosted look only; the
                // light / clear veils keep the picture as it is
                adapt: soft_surface(i as u8) && i as u8 != S_TRACK && t.glass == Glass::Frosted,
            };
            solid[i] = t.solid(i as u8);
        }
        Comp {
            look,
            glass: t.glass,
            br: t.brightness,
            min_c: t.min_contrast,
            s,
            solid,
            track_solid: t.track,
            track_ink: t.track,
        }
    }

    #[inline]
    fn glass(&self, px: Rgb, s: &SurfF, a: f32) -> Rgb {
        let p = f3(px);
        // bright backdrop under a panel (a sun, an aurora): denser tint for every cell
        // alike, so text keeps its contrast without per-character dark plates
        let a = if s.adapt {
            let l = (0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]) / 255.0 * self.br;
            a + (1.0 - a) * ((l - 0.18) * 1.4).clamp(0.0, 0.8)
        } else {
            a
        };
        let k = self.br * (1.0 - a);
        let q = |i: usize| (p[i] * k + s.tint[i] * a + 0.5).clamp(0.0, 255.0) as u8;
        rgb(q(0), q(1), q(2))
    }

    /// Text over the backdrop: (fg, bg, plate). The cell shows the picture's colour
    /// through the glass, and the text keeps its colour where that reads (4.5:1); where
    /// it doesn't, the text gets lighter (same hue, in eighths) — decided on the
    /// picture around the cell too (`wide`, ±3 cells of the row: the lighter of the
    /// two), so the letters of a word change together. Text never turns dark: dark text
    /// takes on the picture's look and sinks into it. Only where even white can't
    /// read (a bright picture without the light glass's cap) the glass thickens
    /// behind it (`plate`).
    fn guarded(&self, fg: Rgb, px: Rgb, s: &SurfF, mut a: f32, wide: Rgb) -> (Rgb, Rgb, bool) {
        let target = self.min_c;
        let mut bg = self.glass(px, s, a);
        let near = if self.glass != Glass::Frosted {
            let w = self.glass(wide, s, a);
            if lum(w) > lum(bg) {
                w
            } else {
                bg
            }
        } else {
            bg
        };
        let f = lighten_to(fg, near, target);
        if contrast(f, bg) >= target {
            return (f, bg, false);
        }
        while a < 1.0 && contrast(f, bg) < target {
            a = (a + 0.1).min(1.0);
            bg = self.glass(px, s, a);
        }
        (f, bg, true)
    }

    /// A mark (frame, graph dot, bar) over the picture: lightened to 3:1 against it.
    #[inline]
    fn mark(&self, fg: Rgb, bg: Rgb) -> Rgb {
        lighten_to(fg, bg, MARK_MIN)
    }

    /// The ink colour of cell `c` drawn into a pixel patch: a bar's track as it is
    /// (decoration), anything else made 3:1 against the picture behind the cell.
    pub fn ink(&self, c: Cell, x: usize, y: usize, bd: &Backdrop) -> Rgb {
        if c.fg & TRACK != 0 {
            return self.track_ink;
        }
        let fg = c.fg & 0xFF_FFFF;
        if c.fg & BLEND != 0 || x >= bd.w || y >= bd.h {
            return fg;
        }
        let sid = (c.bg >> 24) as u8;
        let s = &self.s[sid as usize & 7];
        let cv = match (&bd.soft, soft_surface(sid)) {
            (Some(v), true) => v,
            _ => &bd.sharp,
        };
        let p = cv[y * bd.w + x];
        let px = mix(rgb(p[0], p[1], p[2]), rgb(p[3], p[4], p[5]), 0.5);
        self.mark(fg, self.glass(px, s, s.a))
    }

    /// Composite one logical cell. `nb_blank`: both horizontal neighbours are blank
    /// (a background star may be drawn here).
    pub fn cell(&self, c: Cell, x: usize, y: usize, bd: Option<&Backdrop>, nb_blank: bool) -> Out {
        let sid = (c.bg >> 24) as u8;
        let bold = c.fg & BOLD != 0;
        let fg_rgb = c.fg & 0xFF_FFFF;
        match self.look {
            Look::Solid | Look::Transparent => {
                let transparent = self.look == Look::Transparent;
                let bg = if sid == 0 {
                    c.bg & 0xFF_FFFF
                } else if transparent && matches!(sid, S_SCREEN | S_BAR | S_PANEL) {
                    DEFAULT
                } else {
                    self.solid[sid as usize & 7]
                };
                let fg = if c.fg & TRACK != 0 {
                    self.track_solid
                } else if c.fg & BLEND != 0 {
                    let m = mix_lin(self.track_solid, fg_rgb, blend_k(c.fg));
                    if c.ch == '█' {
                        // a graph's fill: a quiet tint of the curve's colour (mixed in
                        // sRGB: a linear-light mix of a light colour stays bright)
                        mix(self.track_solid, fg_rgb, blend_k(c.fg))
                    } else if bg & DEFAULT != 0 {
                        m
                    } else {
                        // a bar's partly covered end: as clear as the bar (3:1), its
                        // coverage still showing in how strong it is above that
                        lighten_to(m, bg, MARK_MIN)
                    }
                } else if sid != 0 && c.ch != ' ' && bg & DEFAULT == 0 {
                    // legible on its surface (the palette already is, on the plain
                    // surfaces; this catches the rest)
                    lighten_to(fg_rgb, bg, if is_text(c.ch) { self.min_c } else { MARK_MIN })
                } else {
                    fg_rgb
                };
                Out {
                    ch: c.ch,
                    fg,
                    bg,
                    bold,
                    pix: false,
                    plate: false,
                }
            }
            Look::Glass | Look::KittyGlass => {
                let Some(bd) = bd else {
                    return Comp {
                        look: Look::Solid,
                        ..*self
                    }
                    .cell(c, x, y, None, nb_blank);
                };
                let pix = |s: u8| -> (Rgb, Rgb) {
                    if x >= bd.w || y >= bd.h {
                        return (0, 0);
                    }
                    let cv = match (&bd.soft, soft_surface(s)) {
                        (Some(v), true) => v,
                        _ => &bd.sharp,
                    };
                    let p = cv[y * bd.w + x];
                    (rgb(p[0], p[1], p[2]), rgb(p[3], p[4], p[5]))
                };
                // the picture around a text cell (±3 cells of its row) through the glass:
                // bright → dark text (Comp::guarded)
                // the picture around a text cell (Backdrop::wide)
                let wide = bd.wide.get(y * bd.w + x).copied().unwrap_or_else(|| {
                    let (t, b) = pix(sid);
                    mix(t, b, 0.5)
                });
                let kitty = self.look == Look::KittyGlass;
                let mut out = Out {
                    ch: c.ch,
                    fg: fg_rgb,
                    bg: c.bg & 0xFF_FFFF,
                    bold,
                    pix: false,
                    plate: false,
                };
                if sid != 0 {
                    let s = &self.s[sid as usize & 7];
                    if kitty {
                        out.bg = if matches!(sid, S_CHIP | S_SELECT) {
                            mix(self.solid[sid as usize], s.tint_rgb, 0.5)
                        } else {
                            DEFAULT
                        };
                        if c.ch == ' ' {
                            return out;
                        }
                        if out.bg == DEFAULT && c.fg & (TRACK | BLEND | FILL) == 0 {
                            // text straight on the picture: its lightness adapts to the
                            // picture (Comp::guarded), a plate only where that can't help
                            let (top, bot) = pix(sid);
                            let a = self.overlay(sid).1;
                            if is_text(c.ch) {
                                let (f, b, plate) =
                                    self.guarded(fg_rgb, mix(top, bot, 0.5), s, a, wide);
                                // text straight on the picture, or a plate
                                out.fg = f;
                                if plate {
                                    out.bg = b;
                                    out.plate = true;
                                }
                            } else {
                                out.fg = self.mark(fg_rgb, self.glass(mix(top, bot, 0.5), s, a));
                            }
                        }
                    } else {
                        let (top, bot) = pix(sid);
                        if c.ch == ' ' {
                            if bd.has_glyphs
                                && nb_blank
                                && c.fg & STAR_OK != 0
                                && matches!(sid, S_SCREEN | S_PANEL | S_BAR)
                            {
                                if let Some(&(g, gc)) = bd.glyph.get(y * bd.w + x) {
                                    if g != '\0' {
                                        let bgc = self.glass(mix(top, bot, 0.5), s, s.a);
                                        return Out {
                                            ch: g,
                                            fg: mix(bgc, gc, 0.85 * (1.0 - s.a * 0.6)),
                                            bg: bgc,
                                            bold: false,
                                            pix: false,
                                            plate: false,
                                        };
                                    }
                                }
                            }
                            // the picture drawn with block glyphs (soft surfaces of the
                            // frosted look: blurred, half blocks are enough)
                            let i = y * bd.w + x;
                            if !bd.mos.is_empty() && !(bd.soft.is_some() && soft_surface(sid)) {
                                let m = bd.mos[i];
                                let bg = self.glass(rgb(m.bg[0], m.bg[1], m.bg[2]), s, s.a);
                                if let (true, Some(g)) = (m.p != 0, bd.glyphs.char(m.p)) {
                                    return Out {
                                        ch: g,
                                        fg: self.glass(rgb(m.fg[0], m.fg[1], m.fg[2]), s, s.a),
                                        bg,
                                        bold: false,
                                        pix: true,
                                        plate: false,
                                    };
                                }
                                return Out {
                                    ch: ' ',
                                    fg: bg,
                                    bg,
                                    bold: false,
                                    pix: true,
                                    plate: false,
                                };
                            }
                            return Out {
                                ch: '▀',
                                fg: self.glass(top, s, s.a),
                                bg: self.glass(bot, s, s.a),
                                bold: false,
                                pix: true,
                                plate: false,
                            };
                        }
                        out.bg = match c.ch {
                            '▄' => self.glass(top, s, s.a),
                            '▀' => self.glass(bot, s, s.a),
                            '\u{2800}'..='\u{28ff}' | '\u{2500}'..='\u{257f}' | '·' => {
                                // thin glyphs (graph dots, borders): no plate, so they don't
                                // leave dark squares on bright backgrounds
                                self.glass(mix(top, bot, 0.5), s, s.a)
                            }
                            '\u{2581}'..='\u{2587}' => self.glass(top, s, s.a),
                            // text: the surface's own alpha (no plate), denser only where the
                            // contrast guard needs it; adaptive glass already darkens bright spots
                            _ => {
                                let a = if s.adapt { s.a } else { s.ta };
                                let (f, b, plate) =
                                    self.guarded(fg_rgb, mix(top, bot, 0.5), s, a, wide);
                                out.fg = f;
                                out.plate = plate;
                                b
                            }
                        };
                        if c.fg & FILL != 0 {
                            out.fg = fg_rgb;
                        } else if !is_text(c.ch) && c.fg & (TRACK | BLEND) == 0 {
                            // frames, graph dots, bars: 3:1 against the picture
                            out.fg = self.mark(fg_rgb, out.bg);
                        }
                        // frames and grid lines: a pixel patch draws them over the picture
                        // itself (bars' tracks too)
                        out.pix = if c.fg & BLEND != 0 {
                            false
                        } else if c.fg & TRACK != 0 {
                            matches!(c.ch, '▁'..='▄')
                        } else {
                            // (not blocks nor braille: graph cells, drawn as text they
                            // look the same in every frame, while a cell waiting for its
                            // patch would show one look and then another as a graph
                            // scrolls)
                            ink_glyph(c.ch) && !matches!(c.ch, '\u{2800}'..='\u{28ff}')
                        };
                    }
                }
                if c.fg & (TRACK | BLEND) != 0 {
                    let ts = &self.s[S_TRACK as usize];
                    let (top, bot) = pix(S_TRACK);
                    let px = match c.ch {
                        '▄' => bot,
                        '▀' => top,
                        _ => mix(top, bot, 0.5),
                    };
                    let trk = self.glass(px, ts, ts.a);
                    out.fg = if c.fg & TRACK != 0 {
                        trk
                    } else {
                        let m = mix_lin(trk, fg_rgb, blend_k(c.fg));
                        if c.ch == '█' {
                            // a graph's fill: a quiet tint of the curve's colour (mixed
                            // in sRGB: a linear-light mix of a light colour stays bright)
                            mix(trk, fg_rgb, blend_k(c.fg))
                        } else if out.bg & DEFAULT != 0 {
                            m
                        } else {
                            // a bar's partly covered end: 3:1 like the bar, its coverage
                            // still showing in how strong it is above that
                            self.mark(m, out.bg)
                        }
                    };
                }
                out
            }
        }
    }

    /// Glass of a hi-res palette (sixel/iterm patches for blank cells of surface `sid`).
    pub fn glass_palette(&self, pal: &[Rgb], sid: u8) -> Vec<Rgb> {
        let s = &self.s[sid as usize & 7];
        pal.iter().map(|&p| self.glass(p, s, s.a)).collect()
    }

    /// Pixel-patch class of each surface: surfaces whose blank cells look alike (same
    /// glass, same picture) share one, named by the lowest such surface id; 0 = never
    /// patched (dialogs: the menu previews in half blocks).
    pub fn classes(&self, bd: &Backdrop) -> [u8; 8] {
        let key = |i: usize| {
            let s = &self.s[i];
            (
                bd.soft.is_some() && soft_surface(i as u8),
                s.tint_rgb,
                s.a.to_bits(),
                s.adapt,
            )
        };
        let mut cl = [0u8; 8];
        for (i, c) in cl.iter_mut().enumerate().skip(1) {
            if i as u8 != S_DIALOG {
                *c = (1..=i)
                    .find(|&r| r as u8 != S_DIALOG && key(r) == key(i))
                    .unwrap_or(i) as u8;
            }
        }
        cl
    }

    /// kitty: tint and alpha of the overlay placed over a surface's rectangle (frosted
    /// panels use their text alpha: no per-cell plates there)
    pub fn overlay(&self, sid: u8) -> (Rgb, f32) {
        let s = &self.s[sid as usize & 7];
        let text = matches!(sid, S_PANEL | S_DIALOG) && self.glass == Glass::Frosted;
        (s.tint_rgb, if text { s.ta } else { s.a })
    }
}

/// Pixel patch protocol for blank cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Patch {
    None,
    Sixel,
    Iterm,
}

/// When cells that just turned blank get their pixel patch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum When {
    /// in this flush (eco: few cells change, and rarely); also a tick for the cells
    /// of busy areas (NOPATCH) that are waiting
    Now,
    /// not in this flush: remember them (smooth mode, between patch ticks)
    Later,
    /// a patch tick: patch the cells that have stayed blank since the previous tick
    Tick,
}

/// Ticks a blank cell of a busy area (graphs, bars: NOPATCH) must stay unchanged
/// before it gets pixels, and how many such cells one tick may patch (bounded cost).
/// (Smooth: ticks 4 a second. Eco: every flush is a tick, and flushes are 1–10 s
/// apart: a cell that stayed put through one whole sample goes at the next.)
pub const BUSY_TICKS: u8 = 3;
pub const BUSY_PER_TICK: usize = 96;
pub const BUSY_TICKS_ECO: u8 = 1;
pub const BUSY_PER_TICK_ECO: usize = 128;

/// Append a decimal number (no core::fmt on the hot path).
#[inline]
pub fn dec(out: &mut Vec<u8>, mut v: u32) {
    let mut b = [0u8; 10];
    let mut i = b.len();
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&b[i..]);
}

/// Palette index left unset (transparent) in full-screen pictures, and the ink colours
/// (frames and bars drawn into patches): hi-res pictures have at most 190 colours,
/// each picture or patch defines up to 65 inks of its own.
pub const CLEAR: u8 = 255;
pub const INK: u8 = 190;
pub const PAL_MAX: usize = 190;
const INK_SLOTS: usize = CLEAR as usize - INK as usize;

/// The ink colours of one picture or patch.
#[derive(Default)]
struct Inks(Vec<Rgb>);

impl Inks {
    /// Palette index of colour `c`: added if there is room; else `None` (`near`:
    /// the closest one there is).
    fn idx(&mut self, c: Rgb, near: bool) -> Option<u8> {
        if let Some(i) = self.0.iter().position(|&k| k == c) {
            return Some(INK + i as u8);
        }
        if self.0.len() < INK_SLOTS {
            self.0.push(c);
            return Some(INK + self.0.len() as u8 - 1);
        }
        if !near {
            return None;
        }
        let d = |k: Rgb| {
            let (a, b) = (ch(k), ch(c));
            (0..3)
                .map(|j| (a[j] as i32 - b[j] as i32).pow(2))
                .sum::<i32>()
        };
        let (i, _) = self.0.iter().enumerate().min_by_key(|(_, &k)| d(k))?;
        Some(INK + i as u8)
    }
}

/// Glyphs a pixel patch draws itself (lines and dots of the panels' frames).
pub fn ink_glyph(c: char) -> bool {
    matches!(
        c,
        '─' | '│'
            | '╭'
            | '╮'
            | '╰'
            | '╯'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '┃'
            | '━'
            | '╵'
            | '·'
            | '\u{2801}'..='\u{28ff}'
    )
}

/// A character of text (not a blank, line, block or mosaic glyph).
fn is_text(c: char) -> bool {
    c != ' '
        && !ink_glyph(c)
        && !matches!(c, '\u{2580}'..='\u{259f}' | '\u{2800}'..='\u{28ff}' | '\u{1fb00}'..='\u{1fbff}' | '\u{1cd00}'..='\u{1cdff}')
}

/// Draw an ink glyph into a palette-indexed cell block (`w × h` at `(ox, oy)` of a
/// buffer `stride` wide): a centred line one pixel per 10 wide (heavy: double), arcs
/// for the rounded corners, small squares for dots and braille.
#[allow(clippy::too_many_arguments)]
pub fn draw_glyph(
    c: char,
    ink: u8,
    px: &mut [u8],
    stride: usize,
    ox: usize,
    oy: usize,
    w: usize,
    h: usize,
) {
    let heavy = matches!(c, '┃' | '━');
    let t = (w.div_ceil(10)).max(1) * if heavy { 2 } else { 1 };
    let (x0, y0) = ((w - t.min(w)) / 2, (h - t.min(h)) / 2);
    let mut set = |x: usize, y: usize| {
        if x < w && y < h {
            px[(oy + y) * stride + ox + x] = ink;
        }
    };
    if let '\u{2801}'..='\u{28ff}' = c {
        // braille (graph grid lines): dots on a 2 × 4 grid
        const AT: [(usize, usize); 8] = [
            (0, 0),
            (0, 1),
            (0, 2),
            (1, 0),
            (1, 1),
            (1, 2),
            (0, 3),
            (1, 3),
        ];
        let bits = c as u32 - 0x2800;
        if bits.count_ones() > 1 {
            // a graph's edge: each dot fills its eighth of the cell, so the area
            // under the curve is solid (single dots are grid lines: dots)
            for (k, &(gx, gy)) in AT.iter().enumerate() {
                if bits >> k & 1 == 1 {
                    for y in h * gy / 4..h * (gy + 1) / 4 {
                        for x in w * gx / 2..w * (gx + 1) / 2 {
                            set(x, y);
                        }
                    }
                }
            }
            return;
        }
        let d = (w / 5).max(1);
        for (k, &(gx, gy)) in AT.iter().enumerate() {
            if bits >> k & 1 == 1 {
                let x = (w * (1 + 2 * gx)) / 4 - d / 2;
                let y = (h * (1 + 2 * gy)) / 8 - d / 2;
                for yy in y..y + d {
                    for xx in x..x + d {
                        set(xx, yy);
                    }
                }
            }
        }
        return;
    }
    if c == '·' {
        let d = t + 1;
        let (dx, dy) = ((w - d.min(w)) / 2, (h - d.min(h)) / 2);
        for y in dy..dy + d {
            for x in dx..dx + d {
                set(x, y);
            }
        }
        return;
    }
    // arms from the centre: left, right, up, down
    let (l, r, u, d) = match c {
        '─' | '━' => (true, true, false, false),
        '│' | '┃' => (false, false, true, true),
        '├' => (false, true, true, true),
        '┤' => (true, false, true, true),
        '┬' => (true, true, false, true),
        '┴' => (true, true, true, false),
        '┼' => (true, true, true, true),
        '╵' => (false, false, true, false),
        _ => (false, false, false, false),
    };
    let hline = |set: &mut dyn FnMut(usize, usize), a: usize, b: usize| {
        for y in y0..y0 + t {
            for x in a..b {
                set(x, y);
            }
        }
    };
    let vline = |set: &mut dyn FnMut(usize, usize), a: usize, b: usize| {
        for y in a..b {
            for x in x0..x0 + t {
                set(x, y);
            }
        }
    };
    if l {
        hline(&mut set, 0, x0 + t);
    }
    if r {
        hline(&mut set, x0, w);
    }
    if u {
        vline(&mut set, 0, y0 + t);
    }
    if d {
        vline(&mut set, y0, h);
    }
    // rounded corners: a quarter circle joining the two arms
    let arc = match c {
        '╭' => Some((true, true)),   // arms right and down
        '╮' => Some((false, true)),  // left and down
        '╰' => Some((true, false)),  // right and up
        '╯' => Some((false, false)), // left and up
        _ => None,
    };
    if let Some((right, down)) = arc {
        let rad = (w / 2).max(2) as f32;
        // the circle's centre, in pixel-centre coordinates of the line's middle
        let mx = x0 as f32 + (t as f32 - 1.0) / 2.0;
        let my = y0 as f32 + (t as f32 - 1.0) / 2.0;
        let cx = if right { mx + rad } else { mx - rad };
        let cy = if down { my + rad } else { my - rad };
        // the straight parts beyond the arc
        if right {
            hline(&mut set, (cx.round() as usize).min(w), w);
        } else {
            hline(&mut set, 0, (cx.round().max(0.0) as usize + 1).min(w));
        }
        if down {
            vline(&mut set, (cy.round() as usize).min(h), h);
        } else {
            vline(&mut set, 0, (cy.round().max(0.0) as usize + 1).min(h));
        }
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32, y as f32);
                let inq = (if right { fx <= cx } else { fx >= cx })
                    && (if down { fy <= cy } else { fy >= cy });
                if inq {
                    let dd = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                    if (dd - rad).abs() <= t as f32 / 2.0 + 0.3 {
                        set(x, y);
                    }
                }
            }
        }
    }
}

/// The picture's colours through a class's glass, padded to the first ink.
fn glass_pal(comp: &Comp, pal: &[Rgb], class: u8) -> Vec<Rgb> {
    let mut p = comp.glass_palette(pal, class);
    p.resize(INK as usize, 0);
    p
}

/// What a pixel patch draws in cell `c` with ink `ix`: a bar or its track (the lower
/// k/8 of the cell), else a frame / dot / braille glyph.
#[allow(clippy::too_many_arguments)]
fn ink_cell(
    c: Cell,
    ix: u8,
    px: &mut [u8],
    stride: usize,
    ox: usize,
    oy: usize,
    cw: usize,
    chh: usize,
) {
    if let '▁'..='█' = c.ch {
        let k = (c.ch as u32 - 0x2580) as usize;
        // a track is a thin line along the bar's base: the picture shows above it
        let top = if c.fg & TRACK != 0 {
            chh - (chh / 10).max(1)
        } else {
            chh - (chh * k).div_ceil(8)
        };
        for y in top..chh {
            let r = (oy + y) * stride + ox;
            px[r..r + cw].fill(ix);
        }
    } else {
        draw_glyph(c.ch, ix, px, stride, ox, oy, cw, chh);
    }
}

/// The pixels (x0, y0, w, h) of the screen's pixel grid (`pw × ph`) from `src`, which
/// may be smaller (stored at the picture's own resolution): nearest-neighbour.
pub fn crop(
    src: &Indexed,
    pw: usize,
    ph: usize,
    r: (usize, usize, usize, usize),
    out: &mut Vec<u8>,
) {
    let (x0, y0, w, h) = r;
    out.clear();
    if src.w == 0 || src.h == 0 {
        out.resize(w * h, 0);
        return;
    }
    if src.w == pw && src.h == ph {
        for y in y0..y0 + h {
            let o = y * src.w + x0;
            out.extend_from_slice(&src.idx[o..o + w]);
        }
        return;
    }
    let xm: Vec<usize> = (x0..x0 + w)
        .map(|x| (x * src.w / pw.max(1)).min(src.w - 1))
        .collect();
    for y in y0..y0 + h {
        let sy = (y * src.h / ph.max(1)).min(src.h - 1);
        let row = &src.idx[sy * src.w..(sy + 1) * src.w];
        out.extend(xm.iter().map(|&sx| row[sx]));
    }
}

pub struct Screen {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<Cell>,
    /// one bit per cell: logical content changed since the last flush
    dirty: Vec<u64>,
    /// what the terminal shows, as 32-bit cell hashes (half the memory of 64-bit
    /// keys; a collision only delays one cell's repaint until it changes again)
    front: Vec<u32>,
    full: bool,
    pub out: Vec<u8>,
    sgr: Option<(u32, u32, bool)>,
    cur: (usize, usize),
    /// cells to patch in this flush (scratch)
    pm: Vec<u64>,
    /// blank cells drawn with glyphs that still want their pixels, and the ticks each
    /// has waited
    want: Vec<u64>,
    age: Vec<u8>,
    /// round robin over `want` (the word to start the next tick at)
    rr: usize,
    rr_next: Option<usize>,
    pub when: When,
    /// patch palettes per class (the picture's colours through that glass, then the
    /// inks), valid until the next full repaint
    pals: [Option<Vec<Rgb>>; 8],
    /// full repaint (sixel): the patch class of each cell under the picture (0: text)
    img_cls: Vec<u8>,
    sixel: gfx::Sixel,
    pub patch_bytes: usize,
    pub tmux: bool,
}

#[inline]
fn bit(v: &[u64], i: usize) -> bool {
    v[i >> 6] >> (i & 63) & 1 == 1
}

impl Screen {
    pub fn new(w: usize, h: usize) -> Screen {
        let words = (w * h).div_ceil(64) + 1;
        Screen {
            w,
            h,
            buf: vec![BLANK; w * h],
            dirty: vec![0; words],
            front: vec![u32::MAX; w * h],
            full: true,
            out: Vec::with_capacity(64 * 1024),
            sgr: None,
            cur: (usize::MAX, 0),
            pm: vec![0; words],
            want: vec![0; words],
            age: vec![0; w * h],
            rr: 0,
            rr_next: None,
            when: When::Now,
            pals: Default::default(),
            img_cls: Vec::new(),
            sixel: gfx::Sixel::default(),
            patch_bytes: 0,
            tmux: false,
        }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        *self = Screen {
            tmux: self.tmux,
            when: self.when,
            ..Screen::new(w, h)
        };
    }

    /// Next flush repaints everything (start, resize, theme change, resume).
    pub fn invalidate(&mut self) {
        self.full = true;
    }

    pub fn clear(&mut self, c: Cell) {
        for i in 0..self.buf.len() {
            self.set(i, c);
        }
    }

    #[inline]
    fn set(&mut self, i: usize, c: Cell) {
        if self.buf[i] != c {
            self.buf[i] = c;
            self.dirty[i >> 6] |= 1 << (i & 63);
        }
    }

    #[inline]
    pub fn put(&mut self, x: usize, y: usize, ch: char, fg: u32, bg: u32) {
        if x < self.w && y < self.h {
            self.set(y * self.w + x, Cell { ch, fg, bg });
        }
    }

    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: Cell) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                self.set(yy * self.w + xx, c);
            }
        }
    }

    /// Set the background (surface) of a rectangle, keeping characters.
    pub fn set_bg(&mut self, x: usize, y: usize, w: usize, h: usize, bg: u32) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                let i = yy * self.w + xx;
                let c = Cell { bg, ..self.buf[i] };
                self.set(i, c);
            }
        }
    }

    /// Write text clipped to `maxx`; returns the x after the text.
    pub fn text(&mut self, x: usize, y: usize, s: &str, fg: u32, bg: u32, maxx: usize) -> usize {
        let mut x = x;
        let lim = maxx.min(self.w);
        if y >= self.h {
            return x;
        }
        let x0 = x;
        let mut last = ' ';
        for c in s.chars() {
            if x >= lim {
                // cut inside a word: end it with an ellipsis, so it reads as cut
                // ("/dev/mapp…", not "/dev/mappe")
                if x - x0 >= 4 && is_text(c) && is_text(last) && c != ' ' && last != ' ' {
                    self.set(y * self.w + x - 1, Cell { ch: '…', fg, bg });
                }
                break;
            }
            self.set(y * self.w + x, Cell { ch: c, fg, bg });
            last = c;
            x += 1;
        }
        x
    }

    fn sgr(&mut self, o: &Out, x: usize, y: usize, depth: Depth) {
        // a space shows no foreground: keep the current one (fewer bytes)
        let fg = match self.sgr {
            Some((f, _, _)) if o.ch == ' ' => f,
            // (256 colours: rounding can eat the contrast the compositor made)
            _ if depth == Depth::C256 && !o.pix && (o.fg | o.bg) & DEFAULT == 0 && o.fg != o.bg => {
                keep_256(o.fg, o.bg, if is_text(o.ch) { TEXT_MIN } else { MARK_MIN }.min(contrast(o.fg, o.bg)))
            }
            _ => o.fg,
        };
        let want = (fg, o.bg, o.bold);
        if self.sgr == Some(want) {
            return;
        }
        let old = self.sgr;
        let out = &mut self.out;
        out.extend_from_slice(b"\x1b[");
        let mut first = true;
        if old.map(|s| s.2) != Some(o.bold) {
            out.extend_from_slice(if o.bold { b"1" } else { b"22" });
            first = false;
        }
        for (is_bg, v, ov) in [
            (false, fg, old.map(|s| s.0)),
            (true, o.bg, old.map(|s| s.1)),
        ] {
            if ov == Some(v) {
                continue;
            }
            if !first {
                out.push(b';');
            }
            first = false;
            if v & DEFAULT != 0 {
                out.extend_from_slice(if is_bg { b"49" } else { b"39" });
            } else if depth == Depth::C256 {
                out.extend_from_slice(if is_bg { b"48;5;" } else { b"38;5;" });
                dec(out, to_256(v, x, y) as u32);
            } else {
                out.extend_from_slice(if is_bg { b"48;2;" } else { b"38;2;" });
                let [r, g, b] = ch(v);
                dec(out, r as u32);
                out.push(b';');
                dec(out, g as u32);
                out.push(b';');
                dec(out, b as u32);
            }
        }
        out.push(b'm');
        self.sgr = Some(want);
    }

    /// Composite cell `i`. A single blank between two characters of text (the space
    /// between words) gets the text's plate, so a label reads as one plate.
    fn composite(&self, comp: &Comp, i: usize, bd: Option<&Backdrop>, stars: bool) -> Out {
        let (x, y) = (i % self.w, i / self.w);
        let c = self.buf[i];
        if c.ch == ' '
            && matches!(comp.look, Look::Glass | Look::KittyGlass)
            && x > 0
            && x + 1 < self.w
        {
            let (l, r) = (self.buf[i - 1], self.buf[i + 1]);
            if is_text(l.ch) && is_text(r.ch) && l.bg == c.bg && r.bg == c.bg {
                let mut o = comp.cell(
                    Cell {
                        ch: 'x',
                        fg: l.fg,
                        bg: c.bg,
                    },
                    x,
                    y,
                    bd,
                    false,
                );
                if o.plate {
                    o.ch = ' ';
                    o.pix = false;
                    return o;
                }
            }
        }
        comp.cell(c, x, y, bd, self.nb_blank(i, stars))
    }

    /// Both horizontal neighbours blank (a background star may be drawn here).
    #[inline]
    fn nb_blank(&self, i: usize, stars: bool) -> bool {
        let x = i % self.w;
        stars && x > 0 && x + 1 < self.w && self.buf[i - 1].ch == ' ' && self.buf[i + 1].ch == ' '
    }

    /// Composite and emit every changed cell. Returns the bytes to write (may be empty).
    pub fn flush(
        &mut self,
        comp: &Comp,
        bd: Option<&Backdrop>,
        depth: Depth,
        patch: Patch,
    ) -> &[u8] {
        self.out.clear();
        self.out.extend_from_slice(b"\x1b[?2026h"); // synchronized update
        let full = self.full;
        let patching = patch != Patch::None && bd.is_some_and(|b| b.hires.is_some());
        if full {
            self.out.extend_from_slice(b"\x1b[0m");
            self.sgr = None;
            self.cur = (usize::MAX, 0);
            if patch != Patch::None {
                // drop any old images (sixel/iterm) by clearing the screen
                self.out.extend_from_slice(b"\x1b[2J");
            }
            self.want.fill(0);
            self.pals = Default::default();
        }
        let classes = match bd {
            Some(b) if patching => comp.classes(b),
            _ => [0; 8],
        };
        let stars = bd.is_some_and(|b| b.has_glyphs) && matches!(comp.look, Look::Glass);
        let (w, h) = (self.w, self.h);
        let n = self.buf.len();
        // a blank cell showing the backdrop that a pixel patch may cover (cells of busy
        // areas, NOPATCH, only once they have stayed blank a while)
        let elig = |c: &Cell, o: &Out, y: usize| {
            o.pix && classes[(c.bg >> 24) as usize & 7] != 0 && y + 2 < h
        };
        // sixel: a full repaint sends the whole picture first; text then erases it
        // cell by cell (any write to a cell drops its image pixels)
        // (not inside tmux: it drops an image as soon as text overwrites any of it)
        let full_img = full && patching && patch == Patch::Sixel && !self.tmux;
        if full_img {
            self.emit_full(comp, bd.unwrap(), &classes, stars);
        }

        if full {
            self.dirty.iter_mut().for_each(|d| *d = !0);
        } else if stars || matches!(comp.look, Look::Glass | Look::KittyGlass) {
            // a background star depends on its neighbours being blank, a plate between
            // two words on them being text: re-check them too
            let mut carry = 0u64;
            for k in 0..self.dirty.len() {
                let d = self.dirty[k];
                let next_lo = self.dirty.get(k + 1).map_or(0, |x| x & 1);
                self.dirty[k] = d | d << 1 | d >> 1 | carry | next_lo << 63;
                carry = d >> 63;
            }
        }
        let mut any_pm = false;
        if patching {
            // cells waiting for pixels: every bit in `want` is a cell still showing its
            // glyph mosaic (any repaint of it cleared the bit). A tick ages them; quiet
            // cells go after a tick, cells of busy areas after BUSY_TICKS(_ECO), at most
            // BUSY_PER_TICK(_ECO) of those per tick
            if self.when != When::Later {
                let (busy_ticks, busy_max) = if self.when == When::Now {
                    (BUSY_TICKS_ECO, BUSY_PER_TICK_ECO)
                } else {
                    (BUSY_TICKS, BUSY_PER_TICK)
                };
                let mut busy = 0;
                // round robin: start where the last tick's budget ran out, so cells
                // that keep changing near the top can't starve the rest
                let nw = self.want.len();
                let start = self.rr % nw.max(1);
                for kk in 0..nw {
                    let k = (start + kk) % nw;
                    if busy >= busy_max && self.rr_next.is_none() {
                        self.rr_next = Some(k);
                    }
                    let mut bits = self.want[k];
                    while bits != 0 {
                        let b = bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        let i = k * 64 + b;
                        let quiet = self.buf[i].fg & (NOPATCH | TRACK) == 0;
                        self.age[i] = self.age[i].saturating_add(1);
                        // (smooth: a full tick period blank, so short blinks never cost a patch)
                        let ripe = if quiet {
                            self.when == When::Now || self.age[i] >= 2
                        } else {
                            self.age[i] >= busy_ticks && busy < busy_max
                        };
                        if ripe {
                            busy += !quiet as usize;
                            self.want[k] &= !(1u64 << b);
                            self.pm[k] |= 1 << b;
                            any_pm = true;
                        }
                    }
                }
                self.rr = self.rr_next.take().unwrap_or(0);
            }
        }
        for k in 0..self.dirty.len() {
            let mut bits = std::mem::take(&mut self.dirty[k]);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let i = k * 64 + b;
                if i >= n {
                    break;
                }
                let (x, y) = (i % w, i / w);
                let c = self.buf[i];
                let o = self.composite(comp, i, bd, stars);
                let mut e = patching && elig(&c, &o, y);
                let ok = (o.key() >> 32) as u32 ^ o.key() as u32;
                if full_img && e {
                    if self.img_cls[i] != 0 {
                        // the picture is already there
                        self.front[i] = ok;
                        continue;
                    }
                    // left out of the picture (no ink slot): text this time
                    e = false;
                }
                if ok == self.front[i] && !full {
                    continue;
                }
                self.want[k] &= !(1u64 << b);
                self.pm[k] &= !(1u64 << b);
                if e {
                    if full || self.when == When::Now && c.fg & (NOPATCH | TRACK) == 0 {
                        self.pm[k] |= 1 << b;
                        any_pm = true;
                    } else {
                        self.want[k] |= 1 << b;
                        self.age[i] = 0;
                    }
                }
                if self.cur.1 == y && x > self.cur.0 && self.cur.0 < w {
                    // same row, further right: a shorter relative move
                    self.out.extend_from_slice(b"\x1b[");
                    if x - self.cur.0 > 1 {
                        dec(&mut self.out, (x - self.cur.0) as u32);
                    }
                    self.out.push(b'C');
                } else if self.cur != (x, y) {
                    self.out.extend_from_slice(b"\x1b[");
                    dec(&mut self.out, y as u32 + 1);
                    self.out.push(b';');
                    dec(&mut self.out, x as u32 + 1);
                    self.out.push(b'H');
                }
                self.sgr(&o, x, y, depth);
                let mut b = [0u8; 4];
                self.out
                    .extend_from_slice(o.ch.encode_utf8(&mut b).as_bytes());
                self.cur = (x + 1, y);
                self.front[i] = ok;
            }
        }
        self.full = false;
        self.img_cls = Vec::new();
        if patching && any_pm {
            self.emit_patches(comp, bd.unwrap(), patch, &classes);
        }
        if self.out.len() == 8 {
            self.out.clear();
        } else {
            self.out.extend_from_slice(b"\x1b[?2026l");
        }
        &self.out
    }


    /// The source picture of a patch class (frosted panels: the blurred copy).
    fn src<'a>(bd: &Backdrop, hr: &'a Hires, class: u8) -> &'a Indexed {
        match &hr.soft {
            Some(s) if bd.soft.is_some() && soft_surface(class) => s,
            _ => &hr.sharp,
        }
    }

    /// Full repaint (sixel): one picture per patch class over the rows it covers, with
    /// every cell that isn't that class's blank left transparent. Several pictures
    /// compose: unset sixel pixels keep what is there.
    fn emit_full(&mut self, comp: &Comp, bd: &Backdrop, classes: &[u8; 8], stars: bool) {
        let Some(hr) = &bd.hires else { return };
        let (w, h) = (self.w, self.h);
        let (cw, chh) = (hr.cw, hr.ch);
        let before = self.out.len();
        let mut cls = vec![0u8; w * h];
        let mut seen = [false; 8];
        for (i, cl) in cls.iter_mut().enumerate() {
            let y = i / w;
            let c = self.buf[i];
            let o = self.composite(comp, i, Some(bd), stars);
            let k = classes[(c.bg >> 24) as usize & 7];
            if o.pix && k != 0 && y + 2 < h {
                *cl = k;
                seen[k as usize] = true;
            }
        }
        let mut px = Vec::new();
        for k in 1..8u8 {
            if !seen[k as usize] {
                continue;
            }
            let (mut x0, mut x1, mut y0, mut y1) = (w, 0, h, 0);
            for (i, _) in cls.iter().enumerate().filter(|(_, &c)| c == k) {
                let (x, y) = (i % w, i / w);
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
            let src = Self::src(bd, hr, k);
            // inks: a cell whose colour finds no free slot is left to the text
            let mut inks = Inks::default();
            for cy in y0..=y1 {
                for cx in x0..=x1 {
                    let i = cy * w + cx;
                    let c = self.buf[i];
                    if cls[i] == k
                        && c.ch != ' '
                        && inks.idx(comp.ink(c, cx, cy, bd), false).is_none()
                    {
                        cls[i] = 0;
                    }
                }
            }
            let (rw, rh) = ((x1 + 1 - x0) * cw, (y1 + 1 - y0) * chh);
            crop(src, w * cw, h * chh, (x0 * cw, y0 * chh, rw, rh), &mut px);
            for cy in y0..=y1 {
                for cx in x0..=x1 {
                    let c = self.buf[cy * w + cx];
                    let (ox, oy) = ((cx - x0) * cw, (cy - y0) * chh);
                    if cls[cy * w + cx] != k {
                        for yy in 0..chh {
                            let r = (oy + yy) * rw + ox;
                            px[r..r + cw].fill(CLEAR);
                        }
                    } else if c.ch != ' ' {
                        let ix = inks.idx(comp.ink(c, cx, cy, bd), true).unwrap_or(CLEAR);
                        ink_cell(c, ix, &mut px, rw, ox, oy, cw, chh);
                    }
                }
            }
            let mut pal = glass_pal(comp, &src.pal, k);
            pal.extend_from_slice(&inks.0);
            self.out.extend_from_slice(b"\x1b[");
            dec(&mut self.out, y0 as u32 + 1);
            self.out.push(b';');
            dec(&mut self.out, x0 as u32 + 1);
            self.out.push(b'H');
            self.sixel
                .encode(&px, rw, rh, &pal, Some(CLEAR), &mut self.out);
        }
        // the encoder's band buffers grew to the screen's width: give them back
        self.sixel = gfx::Sixel::default();
        // the cells under the picture, for the text pass that follows
        self.img_cls = cls;
        self.cur = (usize::MAX, 0);
        self.patch_bytes += self.out.len() - before;
    }

    /// Patches for the cells marked in `pm`: one per run of adjacent cells of one class
    /// in a row.
    fn emit_patches(&mut self, comp: &Comp, bd: &Backdrop, patch: Patch, classes: &[u8; 8]) {
        let Some(hr) = &bd.hires else { return };
        let before = self.out.len();
        let (w, h) = (self.w, self.h);
        let (cw, chh) = (hr.cw, hr.ch);
        let mut pals = std::mem::take(&mut self.pals);
        let mut px: Vec<u8> = Vec::new();
        let mut seq = Vec::new();
        let n = self.buf.len();
        let mut i = 0;
        while i < n {
            if self.pm[i >> 6] == 0 && i & 63 == 0 {
                i += 64;
                continue;
            }
            if !bit(&self.pm, i) {
                i += 1;
                continue;
            }
            let k = classes[(self.buf[i].bg >> 24) as usize & 7];
            let (x0, y) = (i % w, i / w);
            let mut x1 = x0 + 1;
            while x1 < w
                && bit(&self.pm, y * w + x1)
                && classes[(self.buf[y * w + x1].bg >> 24) as usize & 7] == k
            {
                x1 += 1;
            }
            i = y * w + x1;
            let src = Self::src(bd, hr, k);
            let pw = (x1 - x0) * cw;
            crop(src, w * cw, h * chh, (x0 * cw, y * chh, pw, chh), &mut px);
            let mut inks = Inks::default();
            for x in x0..x1 {
                let c = self.buf[y * w + x];
                if c.ch != ' ' {
                    if let Some(ix) = inks.idx(comp.ink(c, x, y, bd), true) {
                        ink_cell(c, ix, &mut px, pw, (x - x0) * cw, 0, cw, chh);
                    }
                }
            }
            let base = pals[k as usize].get_or_insert_with(|| glass_pal(comp, &src.pal, k));
            let mut pal = base.clone();
            pal.extend_from_slice(&inks.0);
            let pal = &pal;
            self.out.extend_from_slice(b"\x1b[");
            dec(&mut self.out, y as u32 + 1);
            self.out.push(b';');
            dec(&mut self.out, x0 as u32 + 1);
            self.out.push(b'H');
            match patch {
                Patch::Sixel => {
                    seq.clear();
                    self.sixel.encode(&px, pw, chh, pal, None, &mut seq);
                    self.out.extend_from_slice(&seq);
                }
                Patch::Iterm => {
                    let mut rgbv = Vec::with_capacity(px.len() * 3);
                    for &c in &px {
                        rgbv.extend_from_slice(&ch(pal[c as usize]));
                    }
                    let png = gfx::png_rgb(&rgbv, pw, chh);
                    gfx::iterm_inline(&png, x1 - x0, 1, &mut self.out, self.tmux);
                }
                Patch::None => {}
            }
            self.cur = (usize::MAX, 0);
        }
        self.pm.fill(0);
        self.pals = pals;
        self.patch_bytes += self.out.len() - before;
    }

    /// Cells still waiting for pixels (tests, diagnostics).
    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.want.iter().map(|a| a.count_ones() as usize).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        crate::theme::load("default").unwrap()
    }

    #[test]
    fn diff_emits_only_changes() {
        let t = theme();
        let comp = Comp::new(&t, Look::Solid);
        let mut s = Screen::new(10, 3);
        s.text(0, 0, "hello", 0xffffff, surf(S_PANEL), 10);
        let first = s.flush(&comp, None, Depth::True, Patch::None).to_vec();
        assert!(first.len() > 30);
        // nothing changed: nothing written
        s.text(0, 0, "hello", 0xffffff, surf(S_PANEL), 10);
        assert!(s.flush(&comp, None, Depth::True, Patch::None).is_empty());
        // one cell changed: one cursor move + one char
        s.put(4, 0, '!', 0xffffff, surf(S_PANEL));
        let o = String::from_utf8(s.flush(&comp, None, Depth::True, Patch::None).to_vec()).unwrap();
        assert!(
            o.starts_with("\x1b[?2026h\x1b[1;5H\x1b[")
                && o.ends_with("m!\x1b[?2026l")
                && o.len() < 60,
            "{o:?}"
        );
        // same style as the previous cell: no SGR at all
        s.put(5, 0, '?', 0xffffff, surf(S_PANEL));
        let o = String::from_utf8(s.flush(&comp, None, Depth::True, Patch::None).to_vec()).unwrap();
        assert_eq!(o, "\x1b[?2026h?\x1b[?2026l"); // cursor is already there
    }

    #[test]
    fn blended_edge_cell_mixes_bar_and_track() {
        let t = theme();
        let comp = Comp::new(&t, Look::Solid);
        let bar = 0xff8000;
        let c = |k: u32| Cell {
            ch: '▄',
            fg: bar | k << BLEND_SHIFT,
            bg: surf(S_PANEL),
        };
        assert_eq!(comp.cell(c(0), 0, 0, None, false).fg, bar);
        assert_eq!(
            comp.cell(c(8), 0, 0, None, false).fg,
            mix_lin(t.track, bar, 0.5)
        );
        let q = comp.cell(c(15), 0, 0, None, false).fg;
        assert_eq!(q, mix_lin(t.track, bar, 15.0 / 16.0));
        // linear light: half coverage is brighter than the sRGB midpoint
        let l = |c: u32| crate::color::lum(c);
        assert!(l(mix_lin(t.track, bar, 0.5)) > l(mix(t.track, bar, 0.5)));
    }

    #[test]
    fn glass_blank_cells_are_half_blocks() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let mut bd = Backdrop::empty(2, 1);
        bd.sharp = vec![px2(0xff0000, 0x0000ff), px2(0x00ff00, 0xffffff)];
        let o = comp.cell(
            Cell {
                ch: ' ',
                fg: 0,
                bg: surf(S_SCREEN),
            },
            0,
            0,
            Some(&bd),
            false,
        );
        assert_eq!(o.ch, '▀');
        // screen alpha is 0: the pixels come through unchanged (brightness 1)
        assert_eq!((o.fg, o.bg), (0xff0000, 0x0000ff));
        // text on a bright pixel gets a plate dark enough for 4.5:1 (or the tint's limit)
        let o = comp.cell(
            Cell {
                ch: 'A',
                fg: t.fg,
                bg: surf(S_PANEL),
            },
            1,
            0,
            Some(&bd),
            false,
        );
        assert!(
            contrast(t.fg, o.bg)
                >= 4.5f32.min(contrast(t.fg, t.surf[S_PANEL as usize].tint) * 0.9) - 0.01
        );
    }

    #[test]
    fn transparent_uses_default_bg() {
        let t = crate::theme::load("transparent").unwrap();
        let comp = Comp::new(&t, Look::Transparent);
        let o = comp.cell(
            Cell {
                ch: 'x',
                fg: 0xffffff,
                bg: surf(S_PANEL),
            },
            0,
            0,
            None,
            false,
        );
        assert_eq!(o.bg, DEFAULT);
        let mut s = Screen::new(2, 1);
        s.put(0, 0, 'x', 0xffffff, surf(S_PANEL));
        let o = String::from_utf8(s.flush(&comp, None, Depth::True, Patch::None).to_vec()).unwrap();
        assert!(o.contains("49"));
        assert!(!o.contains("48;"));
    }

    fn sixel_fixture(w: usize, h: usize, cw: usize, chh: usize) -> Backdrop {
        let mut bd = Backdrop::empty(w, h);
        let n = w * cw * h * chh;
        bd.hires = Some(Hires {
            cw,
            ch: chh,
            sharp: Indexed {
                w: w * cw,
                h: h * chh,
                idx: (0..n).map(|i| (i % 3) as u8).collect(),
                pal: vec![0x102030, 0x405060, 0x708090],
            },
            soft: None,
            kitty: None,
        });
        bd
    }

    fn images(out: &str) -> Vec<&str> {
        out.split("\x1bP0;1;0q").skip(1).collect()
    }

    #[test]
    fn sixel_full_repaint_is_one_picture_then_patches() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let (w, h, cw, chh) = (4usize, 5usize, 2usize, 4usize);
        let bd = sixel_fixture(w, h, cw, chh);
        let mut s = Screen::new(w, h);
        let blank = Cell {
            ch: ' ',
            fg: 0,
            bg: surf(S_PANEL),
        };
        s.fill(0, 0, w, h, blank);
        s.put(1, 1, ' ', NOPATCH, surf(S_PANEL));
        s.put(2, 0, 'x', 0xffffff, surf(S_PANEL));
        let out = String::from_utf8_lossy(s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel))
            .to_string();
        // one picture over rows 0..=2 (never the last two rows), sent before the text
        let imgs = images(&out);
        assert_eq!(imgs.len(), 1, "{out:?}");
        assert!(out.contains("\x1b[1;1H\x1bP0;1;0q\"1;1;8;12"), "{out:?}");
        let (pic, text) = out.split_at(out.find("\x1b\\").unwrap());
        assert!(!pic.contains('x') && text.contains('x'));
        // the text cell is left transparent in the picture, the busy blank is not
        let seq = &out[out.find("\x1bP0;1;0q").unwrap()..];
        let seq = &seq[..seq.find("\x1b\\").unwrap() + 2];
        let (dw, dh, px) = crate::gfx::sixel_decode(seq.as_bytes());
        assert_eq!((dw, dh), (8, 12));
        let at = |x: usize, y: usize| px[y * dw + x];
        assert!(at(4, 0).is_none() && at(5, 3).is_none());
        assert!(at(2, 4).is_some() && at(0, 0).is_some() && at(7, 11).is_some());
        // cells under the picture get no glyphs: after the picture, the first cell
        // written on row 1 is the text, and rows 3 and 4 are drawn from the start
        assert!(
            text.contains("\x1b[1;3H") && !text.contains("\x1b[1;1H"),
            "{text:?}"
        );
        assert!(text.contains("\x1b[4;1H"), "{text:?}");
        // nothing changed: nothing sent
        assert!(s
            .flush(&comp, Some(&bd), Depth::True, Patch::Sixel)
            .is_empty());
        // text, then blank again: the cell gets its glyph and a one-cell patch
        s.put(0, 2, '7', 0xffffff, surf(S_PANEL));
        let o1 = String::from_utf8_lossy(s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel))
            .to_string();
        assert!(o1.contains('7') && images(&o1).is_empty());
        s.put(0, 2, ' ', 0, surf(S_PANEL));
        let o2 = String::from_utf8_lossy(s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel))
            .to_string();
        assert!(o2.contains("\x1b[3;1H\x1bP0;1;0q\"1;1;2;4"), "{o2:?}");
        let seq = &o2[o2.find("\x1bP0;1;0q").unwrap()..];
        let seq = &seq[..seq.find("\x1b\\").unwrap() + 2];
        let (dw, dh, px) = crate::gfx::sixel_decode(seq.as_bytes());
        assert_eq!((dw, dh), (2, 4));
        assert!(px.iter().all(|p| p.is_some()));
        // a busy cell (NOPATCH) that turns blank waits for the next flush (eco)
        s.put(1, 1, '5', 0xffffff, surf(S_PANEL));
        s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel);
        s.put(1, 1, ' ', NOPATCH, surf(S_PANEL));
        let mut got = Vec::new();
        for _ in 0..BUSY_TICKS_ECO + 1 {
            got.push(
                images(&String::from_utf8_lossy(s.flush(
                    &comp,
                    Some(&bd),
                    Depth::True,
                    Patch::Sixel,
                )))
                .len(),
            );
        }
        assert_eq!(got.iter().sum::<usize>(), 1, "{got:?}");
        assert_eq!(got[BUSY_TICKS_ECO as usize], 1, "{got:?}");
        assert_eq!(s.waiting(), 0);
    }

    #[test]
    fn tmux_gets_runs_not_one_picture() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let bd = sixel_fixture(4, 5, 2, 4);
        let mut s = Screen::new(4, 5);
        s.tmux = true;
        s.fill(
            0,
            0,
            4,
            5,
            Cell {
                ch: ' ',
                fg: 0,
                bg: surf(S_PANEL),
            },
        );
        s.put(2, 0, 'x', 0xffffff, surf(S_PANEL));
        let out = String::from_utf8_lossy(s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel))
            .to_string();
        // row 0 split by the text, rows 1 and 2 whole; the text comes first
        assert_eq!(images(&out).len(), 4, "{out:?}");
        assert!(out.find('x').unwrap() < out.find("\x1bP0;1;0q").unwrap());
    }

    #[test]
    fn smooth_patches_wait_for_a_quiet_tick() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let bd = sixel_fixture(4, 5, 2, 4);
        let mut s = Screen::new(4, 5);
        s.fill(
            0,
            0,
            4,
            5,
            Cell {
                ch: ' ',
                fg: 0,
                bg: surf(S_PANEL),
            },
        );
        s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel);
        let step = |s: &mut Screen, when: When| {
            s.when = when;
            images(&String::from_utf8_lossy(s.flush(
                &comp,
                Some(&bd),
                Depth::True,
                Patch::Sixel,
            )))
            .len()
        };
        s.put(1, 1, '9', 0xffffff, surf(S_PANEL));
        assert_eq!(step(&mut s, When::Later), 0);
        s.put(1, 1, ' ', 0, surf(S_PANEL));
        assert_eq!(step(&mut s, When::Later), 0);
        assert_eq!(s.waiting(), 1);
        // the first tick only starts the clock; the second patches
        assert_eq!(step(&mut s, When::Tick), 0);
        assert_eq!(step(&mut s, When::Later), 0);
        assert_eq!(step(&mut s, When::Tick), 1);
        assert_eq!(s.waiting(), 0);
        // a cell that blanks and returns between ticks never costs a patch
        s.put(2, 2, '3', 0xffffff, surf(S_PANEL));
        step(&mut s, When::Later);
        s.put(2, 2, ' ', 0, surf(S_PANEL));
        step(&mut s, When::Tick);
        s.put(2, 2, '3', 0xffffff, surf(S_PANEL));
        assert_eq!(step(&mut s, When::Tick), 0);
        assert_eq!(s.waiting(), 0);
    }

    #[test]
    fn borders_are_drawn_into_patches_and_bars_stay_text() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let (w, h) = (80usize, 5usize);
        let bd = sixel_fixture(w, h, 10, 20);
        let mut s = Screen::new(w, h);
        s.fill(
            0,
            0,
            w,
            h,
            Cell {
                ch: ' ',
                fg: 0,
                bg: surf(S_PANEL),
            },
        );
        s.put(1, 0, '─', t.border, surf(S_PANEL));
        // a bar and an anti-aliased end: text (blocks look the same in every frame
        // as text, while a patch would replace a text cell some frames later)
        s.put(2, 0, '▂', 0x123456, surf(S_PANEL));
        s.put(3, 0, '▂', 0x654321 | 8 << BLEND_SHIFT, surf(S_PANEL));
        // 70 more colours of lines on row 1: the inks run out, the rest stay text
        for x in 0..70 {
            s.put(x, 1, '│', 0xa0c0c0 + x as u32, surf(S_PANEL));
        }
        let out = String::from_utf8_lossy(s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel))
            .to_string();
        let seq = &out[out.find("\x1bP0;1;0q").unwrap()..];
        let seq = &seq[..seq.find("\x1b\\").unwrap() + 2];
        // the border colour is the first ink of the picture
        let def = |c: u32, ix: u8| {
            let [r, g, b] = crate::color::ch(c);
            let p = |v: u8| (v as u32 * 100 + 127) / 255;
            format!("#{};2;{};{};{}", ix, p(r), p(g), p(b))
        };
        // (each made 3:1 against the picture behind its cell)
        let ink = |ch: char, fg: u32, x: usize| comp.ink(Cell { ch, fg, bg: surf(S_PANEL) }, x, 0, &bd);
        assert!(seq.contains(&def(ink('─', t.border, 1), INK)), "{seq:?}");
        let (dw, _, px) = crate::gfx::sixel_decode(seq.as_bytes());
        // the line: a row of ink across cell 1, picture above and below
        assert!((10..20).all(|x| px[9 * dw + x] == Some(INK)));
        assert!(px[2 * dw + 15] != Some(INK) && px[2 * dw + 15].is_some());
        // the bars: transparent in the picture, written as text afterwards
        assert!(px[16 * dw + 25].is_none() && px[16 * dw + 35].is_none());
        // row 1: 64 inks left, so 64 line cells in the picture and 6 written as text
        let row1 = (0..w)
            .filter(|&x| px[(20 + 10) * dw + x * 10 + 4].is_some_and(|v| v >= INK))
            .count();
        assert_eq!(row1, 64);
        let text = &out[out.find("\x1b\\").unwrap()..];
        assert_eq!(text.matches('─').count(), 0);
        assert_eq!(text.matches('▂').count(), 2);
        assert_eq!(text.matches('│').count(), 6);
    }

    #[test]
    fn glyph_drawing() {
        let (w, h) = (10usize, 20usize);
        let draw = |c: char| {
            let mut px = vec![0u8; w * h];
            draw_glyph(c, 9, &mut px, w, 0, 0, w, h);
            px
        };
        let on = |px: &[u8], x: usize, y: usize| px[y * w + x] == 9;
        let hl = draw('─');
        assert!((0..w).all(|x| on(&hl, x, 9)) && !on(&hl, 0, 8) && !on(&hl, 0, 10));
        let vl = draw('│');
        assert!((0..h).all(|y| on(&vl, 4, y)) && !on(&vl, 3, 0));
        // a rounded corner reaches the right and bottom edges and not the others
        let c = draw('╭');
        assert!(on(&c, w - 1, 9) && on(&c, 4, h - 1));
        assert!(!(0..h).any(|y| on(&c, 0, y)) && !(0..w).any(|x| on(&c, x, 0)));
        // braille: dot 1 top left, dot 8 bottom right
        let b = draw('\u{2881}');
        assert!(on(&b, 2, 2) && on(&b, 7, 17) && !on(&b, 7, 2));
        assert_eq!(draw('·').iter().filter(|&&v| v == 9).count(), 4);
    }

    #[test]
    fn crop_scales_a_smaller_picture() {
        let src = Indexed {
            w: 2,
            h: 2,
            idx: vec![1, 2, 3, 4],
            pal: vec![0; 5],
        };
        let mut out = Vec::new();
        crop(&src, 4, 4, (1, 1, 3, 3), &mut out);
        assert_eq!(out, vec![1, 2, 2, 3, 4, 4, 3, 4, 4]);
        crop(&src, 2, 2, (1, 0, 1, 2), &mut out);
        assert_eq!(out, vec![2, 4]);
    }

    #[test]
    fn light_glass_is_one_patch_class() {
        let mut t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let bd = Backdrop::empty(1, 1);
        let cl = comp.classes(&bd);
        // the default theme's surfaces differ: separate pictures
        assert_ne!(cl[S_PANEL as usize], cl[S_SCREEN as usize]);
        assert_eq!(cl[S_DIALOG as usize], 0);
        for sid in [S_SCREEN, S_BAR, S_PANEL] {
            t.surf[sid as usize] = Surf {
                tint: 0x101010,
                alpha: 0.2,
                text_alpha: 0.4,
            };
        }
        t.glass = Glass::Light;
        let cl = Comp::new(&t, Look::Glass).classes(&bd);
        assert_eq!(cl[S_SCREEN as usize], S_SCREEN);
        assert_eq!(cl[S_BAR as usize], S_SCREEN);
        assert_eq!(cl[S_PANEL as usize], S_SCREEN);
    }

    #[test]
    fn mosaic_blank_cells_and_word_plates() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let mut bd = Backdrop::empty(5, 1);
        bd.glyphs = Glyphs::Octant;
        bd.mos = vec![
            Mos {
                p: 0b0101_0101,
                fg: [255, 0, 0],
                bg: [0, 0, 255],
            };
            5
        ];
        let c = Cell {
            ch: ' ',
            fg: 0,
            bg: surf(S_SCREEN),
        };
        let o = comp.cell(c, 0, 0, Some(&bd), false);
        // screen alpha 0: the colours come through; 0b01010101 = left half
        assert_eq!((o.ch, o.fg, o.bg, o.pix), ('▌', 0xff0000, 0x0000ff, true));
        // a blank between two words on a dark picture stays picture
        let mut s = Screen::new(5, 1);
        s.fill(0, 0, 5, 1, c);
        s.text(0, 0, "ab cd", 0xffffff, surf(S_SCREEN), 5);
        assert!(s.composite(&comp, 2, Some(&bd), false).pix);
        // on a bright one the words get plates (the built-in look), and the blank
        // between them takes the words' plate
        bd.sharp = vec![px2(0xf0e0a0, 0xf0e0a0); 5];
        let o = s.composite(&comp, 2, Some(&bd), false);
        let word = s.composite(&comp, 1, Some(&bd), false);
        assert!(word.plate);
        assert_eq!((o.ch, o.bg, o.pix), (' ', word.bg, false));
    }

    #[test]
    fn text_and_marks_read_on_any_picture() {
        let mut t = theme();
        t.glass = Glass::Light;
        for sid in [S_SCREEN, S_BAR, S_PANEL] {
            t.surf[sid as usize] = Surf {
                tint: 0x101010,
                alpha: 0.1,
                text_alpha: 0.1,
            };
        }
        let comp = Comp::new(&t, Look::Glass);
        let mut bd = Backdrop::empty(1, 1);
        let cell = |ch: char, fg: u32| Cell {
            ch,
            fg,
            bg: surf(S_PANEL),
        };
        let (dim, light) = (0x6a5f78, 0xe0e0f0);
        // a dark sky: the colours as they are
        bd.sharp = vec![px2(0x101830, 0x101830)];
        let o = comp.cell(cell('A', light), 0, 0, Some(&bd), false);
        assert!(!o.plate && o.fg == light);
        // a sun dimmed to the light glass's cap (image::tone): a dim label gets
        // lighter, never darker, and reads at 4.5:1 without a plate
        bd.sharp = vec![px2(0x8a5a1c, 0x8a5a1c)];
        let o = comp.cell(cell('a', dim), 0, 0, Some(&bd), false);
        assert!(!o.plate && contrast(o.fg, o.bg) >= 4.5, "{o:?}");
        assert!(crate::color::lum(o.fg) > crate::color::lum(dim));
        // a frame there: 3:1
        let o = comp.cell(cell('│', 0x4a3a5a), 0, 0, Some(&bd), false);
        assert!(contrast(o.fg, o.bg) >= 3.0, "{o:?}");
        // a full-brightness sun (clear glass): the text stays light and gets a plate
        bd.sharp = vec![px2(0xffd060, 0xffc050)];
        let o = comp.cell(cell('A', light), 0, 0, Some(&bd), false);
        assert!(o.plate && crate::color::lum(o.fg) > 0.5 && contrast(o.fg, o.bg) >= 4.5, "{o:?}");
    }
}

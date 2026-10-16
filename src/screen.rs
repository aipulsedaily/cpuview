//! Cell buffer, the glass compositor (themes/README.md "Surfaces and the glass model")
//! and the diffing emitter.
//!
//! Frames are drawn as *logical* cells (character, colour, surface). Only cells whose
//! logical content changed since the last frame are composited and compared with what
//! the terminal shows, so a typical live frame touches a few hundred cells and writes a
//! few hundred bytes in a single write(). Unchanged frames write nothing.

use crate::color::{ch, contrast, mix, rgb, to_256, Rgb};
use crate::gfx;
use crate::image::Indexed;
use crate::theme::*;
use std::io::Write;

pub const BOLD: u32 = 1 << 24;
/// fg is the `track` surface seen through glass (empty bar segments)
pub const TRACK: u32 = 1 << 25;
/// blank cell in a frequently changing area: never gets a pixel patch (nor a star)
pub const NOPATCH: u32 = 1 << 26;
/// blank fill cell (screen / empty panel area): a background star may be drawn here.
/// Blanks written by widgets never get one, so a star can't be mistaken for data.
pub const STAR_OK: u32 = 1 << 27;
pub const DEFAULT: u32 = 1 << 24;
/// fg bits 28..30 = k (1..7): the colour covers k/8 of the cell and is blended into
/// the track surface (the anti-aliased edge cell of a bar)
pub const BLEND_SHIFT: u32 = 28;
pub const BLEND: u32 = 7 << BLEND_SHIFT;

#[inline]
fn blend_k(fg: u32) -> f32 {
    ((fg & BLEND) >> BLEND_SHIFT) as f32 / 8.0
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

/// Background rendered for the current size: half-block canvas (2 px per cell) and,
/// for pixel modes, a hi-res quantised copy at terminal pixel size.
pub struct Backdrop {
    pub w: usize,
    pub h: usize,
    /// per cell: [upper half pixel, lower half pixel] (interleaved for locality)
    pub sharp: Vec<[Rgb; 2]>,
    pub soft: Option<Vec<[Rgb; 2]>>,
    pub glyph: Vec<(char, Rgb)>,
    pub has_glyphs: bool,
    pub hires: Option<Hires>,
}

pub struct Hires {
    pub cw: usize,
    pub ch: usize,
    pub sharp: Indexed,
    pub soft: Indexed,
    /// kitty: RGB data to transmit once (sharp, soft), dropped after sending
    pub kitty: Option<(Vec<u8>, Vec<u8>)>,
}

impl Backdrop {
    pub fn empty(w: usize, h: usize) -> Backdrop {
        Backdrop {
            w,
            h,
            sharp: vec![[0; 2]; w * h],
            soft: None,
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
    br: f32,
    min_c: f32,
    s: [SurfF; 8],
    solid: [Rgb; 8],
    track_solid: Rgb,
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
                adapt: soft_surface(i as u8) && i as u8 != S_TRACK,
            };
            solid[i] = t.solid(i as u8);
        }
        Comp {
            look,
            br: t.brightness,
            min_c: t.min_contrast,
            s,
            solid,
            track_solid: t.track,
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

    fn guarded(&self, fg: Rgb, px: Rgb, s: &SurfF, mut a: f32, cap: f32) -> Rgb {
        // adaptive (panel) glass is already dark where the backdrop is bright: ask less of
        // the guard there, so dim labels do not get dark plates of their own
        let keep = if s.adapt { 0.75 } else { 0.9 };
        let target = self.min_c.min(cap).min(contrast(fg, s.tint_rgb) * keep);
        let mut bg = self.glass(px, s, a);
        while a < 1.0 && contrast(fg, bg) < target {
            a = (a + 0.1).min(1.0);
            bg = self.glass(px, s, a);
        }
        bg
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
                    mix(self.track_solid, fg_rgb, blend_k(c.fg))
                } else {
                    fg_rgb
                };
                Out {
                    ch: c.ch,
                    fg,
                    bg,
                    bold,
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
                    (p[0], p[1])
                };
                let kitty = self.look == Look::KittyGlass;
                let mut out = Out {
                    ch: c.ch,
                    fg: fg_rgb,
                    bg: c.bg & 0xFF_FFFF,
                    bold,
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
                                        };
                                    }
                                }
                            }
                            return Out {
                                ch: '▀',
                                fg: self.glass(top, s, s.a),
                                bg: self.glass(bot, s, s.a),
                                bold: false,
                            };
                        }
                        out.bg = match c.ch {
                            '▄' => self.glass(top, s, s.a),
                            '▀' => self.glass(bot, s, s.a),
                            '\u{2800}'..='\u{28ff}' | '\u{2500}'..='\u{257f}' => {
                                // thin glyphs (graph dots, borders): no plate, so they don't
                                // leave dark squares on bright backgrounds
                                self.glass(mix(top, bot, 0.5), s, s.a)
                            }
                            '\u{2581}'..='\u{2587}' => self.glass(top, s, s.a),
                            // text: the surface's own alpha (no plate), denser only where the
                            // contrast guard needs it; adaptive glass already darkens bright spots
                            _ => self.guarded(
                                fg_rgb,
                                mix(top, bot, 0.5),
                                s,
                                if s.adapt { s.a } else { s.ta },
                                99.0,
                            ),
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
                        mix(trk, fg_rgb, blend_k(c.fg))
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

    pub fn surf_tint(&self, sid: u8, text: bool) -> (Rgb, f32) {
        let s = &self.s[sid as usize & 7];
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

pub struct Screen {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<Cell>,
    /// one bit per cell: logical content changed since the last flush
    dirty: Vec<u64>,
    /// what the terminal shows, as a 64-bit hash per cell (8 bytes instead of 16)
    /// what the terminal shows, as 32-bit cell hashes (half the memory of 64-bit
    /// keys; a collision only delays one cell's repaint until it changes again)
    front: Vec<u32>,
    full: bool,
    pub out: Vec<u8>,
    sgr: Option<(u32, u32, bool)>,
    cur: (usize, usize),
    marks: Vec<(u16, u16, u8)>,
    sixel: gfx::Sixel,
    pub patch_bytes: usize,
    pub tmux: bool,
}

impl Screen {
    pub fn new(w: usize, h: usize) -> Screen {
        Screen {
            w,
            h,
            buf: vec![BLANK; w * h],
            dirty: vec![0; (w * h).div_ceil(64) + 1],
            front: vec![u32::MAX; w * h],
            full: true,
            out: Vec::with_capacity(64 * 1024),
            sgr: None,
            cur: (usize::MAX, 0),
            marks: Vec::new(),
            sixel: gfx::Sixel::default(),
            patch_bytes: 0,
            tmux: false,
        }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        *self = Screen {
            tmux: self.tmux,
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
        for c in s.chars() {
            if x >= lim {
                break;
            }
            self.set(y * self.w + x, Cell { ch: c, fg, bg });
            x += 1;
        }
        x
    }

    fn sgr(&mut self, o: &Out, x: usize, y: usize, depth: Depth) {
        let want = (o.fg, o.bg, o.bold);
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
            (false, o.fg, old.map(|s| s.0)),
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
        self.marks.clear();
        let full = self.full;
        if full {
            self.out.extend_from_slice(b"\x1b[0m");
            self.sgr = None;
            self.cur = (usize::MAX, 0);
            if patch != Patch::None {
                // drop any old images (sixel/iterm) by clearing the screen
                self.out.extend_from_slice(b"\x1b[2J");
            }
        }
        let stars = bd.is_some_and(|b| b.has_glyphs) && matches!(comp.look, Look::Glass);
        let w = self.w;
        let n = self.buf.len();
        let patching = patch != Patch::None && bd.is_some_and(|b| b.hires.is_some());
        if full {
            self.dirty.iter_mut().for_each(|d| *d = !0);
        } else if stars {
            // a background star depends on its neighbours being blank: re-check them too
            let mut carry = 0u64;
            for k in 0..self.dirty.len() {
                let d = self.dirty[k];
                let next_lo = self.dirty.get(k + 1).map_or(0, |x| x & 1);
                self.dirty[k] = d | d << 1 | d >> 1 | carry | next_lo << 63;
                carry = d >> 63;
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
                let nb = stars
                    && x > 0
                    && x + 1 < w
                    && self.buf[i - 1].ch == ' '
                    && self.buf[i + 1].ch == ' ';
                let o = comp.cell(c, x, y, bd, nb);
                let sid = (c.bg >> 24) as u8;
                if patching
                    && c.ch == ' '
                    && o.ch == '▀'
                    && c.fg & NOPATCH == 0
                    && sid != 0
                    && y + 2 < self.h
                {
                    self.marks.push((x as u16, y as u16, sid));
                }
                let ok = (o.key() >> 32) as u32 ^ o.key() as u32;
                if ok == self.front[i] && !full {
                    continue;
                }
                if self.cur != (x, y) {
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
        if patching && !self.marks.is_empty() {
            self.emit_patches(comp, bd.unwrap(), patch);
        }
        if self.out.len() == 8 {
            self.out.clear();
        } else {
            self.out.extend_from_slice(b"\x1b[?2026l");
        }
        &self.out
    }

    fn emit_patches(&mut self, comp: &Comp, bd: &Backdrop, patch: Patch) {
        let Some(hr) = &bd.hires else { return };
        let before = self.out.len();
        let marks = std::mem::take(&mut self.marks);
        let mut k = 0;
        let mut crop: Vec<u8> = Vec::new();
        while k < marks.len() {
            let (x0, y, s) = marks[k];
            let mut x1 = x0 + 1;
            let mut j = k + 1;
            while j < marks.len() && marks[j] == (x1, y, s) {
                x1 += 1;
                j += 1;
            }
            k = j;
            let src = if soft_surface(s) { &hr.soft } else { &hr.sharp };
            let (cw, chh) = (hr.cw, hr.ch);
            let (px0, py0) = (x0 as usize * cw, y as usize * chh);
            let pw = (x1 - x0) as usize * cw;
            if px0 + pw > src.w || py0 + chh > src.h {
                continue;
            }
            crop.clear();
            for yy in 0..chh {
                let r = (py0 + yy) * src.w + px0;
                crop.extend_from_slice(&src.idx[r..r + pw]);
            }
            let pal = comp.glass_palette(&src.pal, s);
            let _ = write!(self.out, "\x1b[{};{}H", y + 1, x0 + 1);
            match patch {
                Patch::Sixel => {
                    let mut seq = Vec::new();
                    self.sixel.encode(&crop, pw, chh, &pal, None, &mut seq);
                    self.out.extend_from_slice(&seq);
                }
                Patch::Iterm => {
                    let mut rgbv = Vec::with_capacity(crop.len() * 3);
                    for &i in &crop {
                        rgbv.extend_from_slice(&ch(pal[i as usize]));
                    }
                    let png = gfx::png_rgb(&rgbv, pw, chh);
                    gfx::iterm_inline(&png, (x1 - x0) as usize, 1, &mut self.out, self.tmux);
                }
                Patch::None => {}
            }
            self.cur = (usize::MAX, 0);
        }
        self.marks = marks;
        self.patch_bytes += self.out.len() - before;
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
            comp.cell(c(4), 0, 0, None, false).fg,
            mix(t.track, bar, 0.5)
        );
        let q = comp.cell(c(1), 0, 0, None, false).fg;
        assert_eq!(q, mix(t.track, bar, 0.125));
    }

    #[test]
    fn glass_blank_cells_are_half_blocks() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let mut bd = Backdrop::empty(2, 1);
        bd.sharp = vec![[0xff0000, 0x0000ff], [0x00ff00, 0xffffff]];
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

    #[test]
    fn sixel_patches_only_on_changed_blank_cells() {
        let t = theme();
        let comp = Comp::new(&t, Look::Glass);
        let (w, h, cw, chh) = (4usize, 5usize, 2usize, 4usize);
        let mut bd = Backdrop::empty(w, h);
        let n = w * cw * h * chh;
        let ix = |k: usize| Indexed {
            w: w * cw,
            h: h * chh,
            idx: (0..n).map(|i| (i % k) as u8).collect(),
            pal: vec![0x102030, 0x405060, 0x708090],
        };
        bd.hires = Some(Hires {
            cw,
            ch: chh,
            sharp: ix(3),
            soft: ix(2),
            kitty: None,
        });
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
        s.put(1, 1, ' ', NOPATCH, surf(S_PANEL));
        s.put(2, 0, 'x', 0xffffff, surf(S_PANEL));
        let out = String::from_utf8_lossy(
            &s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel)
                .to_vec(),
        )
        .to_string();
        // rows 0..=2 get patches (h - 2 = 3 rows); row 0 is split by the 'x'
        let patches: Vec<&str> = out.split("\x1bP0;1;0q").skip(1).collect();
        assert_eq!(patches.len(), 5, "{out:?}"); // row0: [0..2) and [3..4); row1: [0..1) and [2..4); row2: [0..4)
        assert!(out.contains("\x1b[1;1H\x1bP0;1;0q\"1;1;4;4"));
        assert!(out.contains("\x1b[2;3H\x1bP0;1;0q\"1;1;4;4"));
        assert!(out.contains("\x1b[3;1H\x1bP0;1;0q\"1;1;8;4"));
        assert!(
            !out.contains("\x1b[4;1H\x1bP"),
            "never patch the last two rows"
        );
        // nothing changed: nothing sent
        assert!(s
            .flush(&comp, Some(&bd), Depth::True, Patch::Sixel)
            .is_empty());
        // text then blank again: the cell gets its half-block and a new patch
        s.put(0, 2, '7', 0xffffff, surf(S_PANEL));
        let o1 = String::from_utf8_lossy(
            &s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel)
                .to_vec(),
        )
        .to_string();
        assert!(!o1.contains("\x1bP0;1;0q"));
        s.put(0, 2, ' ', 0, surf(S_PANEL));
        let o2 = String::from_utf8_lossy(
            &s.flush(&comp, Some(&bd), Depth::True, Patch::Sixel)
                .to_vec(),
        )
        .to_string();
        assert!(
            o2.contains("▀") && o2.contains("\x1b[3;1H\x1bP0;1;0q\"1;1;2;4"),
            "{o2:?}"
        );
        // the patch itself decodes to the right size
        let seq = &o2[o2.find("\x1bP0;1;0q").unwrap()..];
        let seq = &seq[..seq.find("\x1b\\").unwrap() + 2];
        let (dw, dh, px) = crate::gfx::sixel_decode(seq.as_bytes());
        assert_eq!((dw, dh), (2, 4));
        assert!(px.iter().all(|p| p.is_some()));
    }
}

//! Terminal graphics encoders: DECSIXEL, kitty graphics protocol, iTerm2 inline images,
//! a minimal (stored-deflate) PNG writer and base64.

use crate::color::{ch, Rgb};
use std::io::Write;

/// Reusable sixel encoder (keeps its scratch buffers between patches).
#[derive(Default)]
pub struct Sixel {
    lines: Vec<u8>,
    slot: Vec<u16>,
    order: Vec<u8>,
    /// per colour of the band: first and last column it appears in
    ext: Vec<(u32, u32)>,
}

impl Sixel {
    /// Encode palette-indexed pixels (`idx.len() == w*h`) as DECSIXEL with P2=1 (unset
    /// pixels transparent). `pal[i]` is colour i; only colours that are used get defined.
    /// Pixels equal to `transparent` are left unset.
    pub fn encode(
        &mut self,
        idx: &[u8],
        w: usize,
        h: usize,
        pal: &[Rgb],
        transparent: Option<u8>,
        out: &mut Vec<u8>,
    ) {
        use crate::screen::dec;
        out.extend_from_slice(b"\x1bP0;1;0q\"1;1;");
        dec(out, w as u32);
        out.push(b';');
        dec(out, h as u32);
        let mut used = [false; 256];
        for &c in idx {
            used[c as usize] = true;
        }
        if let Some(t) = transparent {
            used[t as usize] = false;
        }
        for (c, &u) in used.iter().enumerate() {
            if u {
                let [r, g, b] = ch(*pal.get(c).unwrap_or(&0));
                let p = |v: u8| (v as u32 * 100 + 127) / 255;
                out.push(b'#');
                dec(out, c as u32);
                out.extend_from_slice(b";2;");
                dec(out, p(r));
                out.push(b';');
                dec(out, p(g));
                out.push(b';');
                dec(out, p(b));
            }
        }
        if self.slot.len() < 256 {
            self.slot = vec![u16::MAX; 256];
            self.ext = vec![(0, 0); 256];
        }
        // one line of sixel bits per colour; kept all-zero between bands
        if self.lines.len() < 256 * w {
            self.lines = vec![0; 256 * w];
        }
        let mut y0 = 0;
        while y0 < h {
            let rows = (h - y0).min(6);
            self.order.clear();
            for k in 0..rows {
                let row = &idx[(y0 + k) * w..(y0 + k + 1) * w];
                for (x, &c) in row.iter().enumerate() {
                    if Some(c) == transparent {
                        continue;
                    }
                    let mut s = self.slot[c as usize];
                    let x32 = x as u32;
                    if s == u16::MAX {
                        s = self.order.len() as u16;
                        self.slot[c as usize] = s;
                        self.order.push(c);
                        self.ext[s as usize] = (x32, x32);
                    } else {
                        let e = &mut self.ext[s as usize];
                        e.0 = e.0.min(x32);
                        e.1 = e.1.max(x32);
                    }
                    self.lines[s as usize * w + x] |= 1 << k;
                }
            }
            for (si, &c) in self.order.iter().enumerate() {
                if si > 0 {
                    out.push(b'$');
                }
                out.push(b'#');
                dec(out, c as u32);
                let (x0, x1) = (self.ext[si].0 as usize, self.ext[si].1 as usize);
                if x0 >= 4 {
                    out.push(b'!');
                    dec(out, x0 as u32);
                    out.push(b'?');
                } else {
                    out.extend(std::iter::repeat_n(b'?', x0));
                }
                let line = &mut self.lines[si * w..si * w + w];
                let mut x = x0;
                while x <= x1 {
                    let v = line[x];
                    let mut run = 1;
                    while x + run <= x1 && line[x + run] == v {
                        run += 1;
                    }
                    let chr = 63 + v;
                    if run >= 4 {
                        out.push(b'!');
                        dec(out, run as u32);
                        out.push(chr);
                    } else {
                        out.extend(std::iter::repeat_n(chr, run));
                    }
                    x += run;
                }
                line[x0..=x1].fill(0);
            }
            for &c in &self.order {
                self.slot[c as usize] = u16::MAX;
            }
            y0 += 6;
            if y0 < h {
                out.push(b'-');
            }
        }
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Minimal DECSIXEL decoder (tests): (w, h, pixels) with None = unset.
#[cfg(test)]
pub fn sixel_decode(data: &[u8]) -> (usize, usize, Vec<Option<u8>>) {
    let q = data.iter().position(|&c| c == b'q').unwrap() + 1;
    let end = data.len() - 2;
    let body = &data[q..end];
    let mut i = 0;
    let num = |b: &[u8], i: &mut usize| -> usize {
        let mut v = 0;
        while *i < b.len() && b[*i].is_ascii_digit() {
            v = v * 10 + (b[*i] - b'0') as usize;
            *i += 1;
        }
        v
    };
    assert_eq!(body[0], b'"');
    i += 1;
    let mut ra = [0usize; 4];
    for (k, r) in ra.iter_mut().enumerate() {
        *r = num(body, &mut i);
        if k < 3 {
            i += 1;
        }
    }
    let (w, h) = (ra[2], ra[3]);
    let mut px = vec![None; w * h];
    let (mut x, mut band, mut col) = (0usize, 0usize, 0u8);
    while i < body.len() {
        let c = body[i];
        match c {
            b'#' => {
                i += 1;
                let n = num(body, &mut i);
                if i < body.len() && body[i] == b';' {
                    // colour definition: skip ;2;r;g;b
                    for _ in 0..4 {
                        i += 1;
                        num(body, &mut i);
                    }
                } else {
                    col = n as u8;
                }
                continue;
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                band += 1
            }
            b'!' => {
                i += 1;
                let n = num(body, &mut i);
                let s = body[i] - 63;
                for _ in 0..n {
                    for k in 0..6 {
                        if s >> k & 1 == 1 && band * 6 + k < h {
                            px[(band * 6 + k) * w + x] = Some(col);
                        }
                    }
                    x += 1;
                }
            }
            63..=126 => {
                let s = c - 63;
                for k in 0..6 {
                    if s >> k & 1 == 1 && band * 6 + k < h {
                        px[(band * 6 + k) * w + x] = Some(col);
                    }
                }
                x += 1;
            }
            _ => {}
        }
        i += 1;
    }
    (w, h, px)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8], out: &mut Vec<u8>) {
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63]);
        out.push(B64[(n >> 12) as usize & 63]);
        out.push(if c.len() > 1 {
            B64[(n >> 6) as usize & 63]
        } else {
            b'='
        });
        out.push(if c.len() > 2 {
            B64[n as usize & 63]
        } else {
            b'='
        });
    }
}

/// kitty: transmit raw RGB (f=24) as image `id`, base64 in 4096-byte chunks, q=2 (no replies).
pub fn kitty_transmit_rgb(rgb: &[u8], w: usize, h: usize, id: u32, out: &mut Vec<u8>, tmux: bool) {
    let mut b64 = Vec::with_capacity(rgb.len() * 4 / 3 + 4);
    base64(rgb, &mut b64);
    kitty_chunks(
        &b64,
        &format!("a=t,f=24,s={w},v={h},t=d,i={id},q=2"),
        out,
        tmux,
    );
}

/// kitty: transmit a 1×1 RGBA pixel (tint overlays).
pub fn kitty_transmit_pixel(rgba: [u8; 4], id: u32, out: &mut Vec<u8>, tmux: bool) {
    let mut b64 = Vec::new();
    base64(&rgba, &mut b64);
    kitty_chunks(&b64, &format!("a=t,f=32,s=1,v=1,t=d,i={id},q=2"), out, tmux);
}

fn kitty_chunks(b64: &[u8], ctl: &str, out: &mut Vec<u8>, tmux: bool) {
    let parts: Vec<&[u8]> = if b64.is_empty() {
        vec![b64]
    } else {
        b64.chunks(4096).collect()
    };
    for (n, p) in parts.iter().enumerate() {
        let more = (n + 1 < parts.len()) as u8;
        let mut seq = Vec::with_capacity(p.len() + 64);
        if n == 0 {
            let _ = write!(seq, "\x1b_G{ctl},m={more};");
        } else {
            let _ = write!(seq, "\x1b_Gm={more};");
        }
        seq.extend_from_slice(p);
        seq.extend_from_slice(b"\x1b\\");
        wrap(&seq, out, tmux);
    }
}

/// Place image `id` at cell (x, y) scaled to cols × rows, cursor kept still.
#[allow(clippy::too_many_arguments)]
pub fn kitty_place(
    id: u32,
    pid: u32,
    x: usize,
    y: usize,
    cols: usize,
    rows: usize,
    z: i64,
    out: &mut Vec<u8>,
    tmux: bool,
) {
    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
    let s = format!("\x1b_Ga=p,i={id},p={pid},c={cols},r={rows},z={z},C=1,q=2;\x1b\\");
    wrap(s.as_bytes(), out, tmux);
}

pub fn kitty_delete(id: Option<u32>, out: &mut Vec<u8>, tmux: bool) {
    let s = match id {
        Some(i) => format!("\x1b_Ga=d,d=I,i={i},q=2;\x1b\\"),
        None => "\x1b_Ga=d,d=A,q=2;\x1b\\".to_string(),
    };
    wrap(s.as_bytes(), out, tmux);
}

/// iTerm2 inline image stretched to cols × rows cells; doNotMoveCursor avoids scrolling.
pub fn iterm_inline(png: &[u8], cols: usize, rows: usize, out: &mut Vec<u8>, tmux: bool) {
    let mut s = Vec::with_capacity(png.len() * 4 / 3 + 128);
    let _ = write!(
        s,
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=0;doNotMoveCursor=1:",
        png.len()
    );
    base64(png, &mut s);
    s.push(0x07);
    wrap(&s, out, tmux);
}

/// tmux passthrough (needs `set -g allow-passthrough on`).
pub fn wrap(seq: &[u8], out: &mut Vec<u8>, tmux: bool) {
    if !tmux {
        out.extend_from_slice(seq);
        return;
    }
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in seq {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
}

fn crc32(data: &[u8], mut c: u32) -> u32 {
    c = !c;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// PNG (RGB, 8-bit) with stored (uncompressed) deflate blocks: tiny code, valid PNG.
pub fn png_rgb(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w * 3 + 1) * h);
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * w * 3..(y + 1) * w * 3]);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        let l = b.len() as u16;
        z.extend_from_slice(&l.to_le_bytes());
        z.extend_from_slice(&(!l).to_le_bytes());
        z.extend_from_slice(b);
    }
    if blocks.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let chunk = |out: &mut Vec<u8>, t: &[u8], d: &[u8]| {
        out.extend_from_slice(&(d.len() as u32).to_be_bytes());
        out.extend_from_slice(t);
        out.extend_from_slice(d);
        out.extend_from_slice(&crc32(d, crc32(t, 0)).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 16) as u32
        }
    }

    #[test]
    fn sixel_round_trip() {
        let mut r = Rng(0x9e3779b97f4a7c15);
        let mut enc = Sixel::default();
        for (w, h) in [(1, 1), (7, 5), (37, 13), (64, 20), (200, 6), (300, 20)] {
            let pal: Vec<Rgb> = (0..20).map(|_| r.next() & 0xffffff).collect();
            let idx: Vec<u8> = (0..w * h)
                .map(|_| {
                    if r.next() % 10 < 7 {
                        [0, 0, 0, 3, 7, 19][(r.next() % 6) as usize]
                    } else {
                        (r.next() % 20) as u8
                    }
                })
                .collect();
            for tr in [None, Some(0u8)] {
                let mut out = Vec::new();
                enc.encode(&idx, w, h, &pal, tr, &mut out);
                let (dw, dh, px) = sixel_decode(&out);
                assert_eq!((dw, dh), (w, h));
                for (i, &c) in idx.iter().enumerate() {
                    let want = if Some(c) == tr { None } else { Some(c) };
                    assert_eq!(px[i], want, "{w}x{h} tr={tr:?} at {i}");
                }
            }
        }
        // RLE of a flat run and exact bytes for a tiny image
        let mut out = Vec::new();
        enc.encode(&[1; 10], 10, 1, &[0, 0xff0000], None, &mut out);
        assert_eq!(
            out,
            b"\x1bP0;1;0q\"1;1;10;1#1;2;100;0;0#1!10@\x1b\\".to_vec()
        );
    }

    #[test]
    fn b64_png_kitty() {
        let mut o = Vec::new();
        base64(b"Man", &mut o);
        base64(b"Ma", &mut o);
        base64(b"M", &mut o);
        assert_eq!(o, b"TWFuTWE=TQ==".to_vec());
        let png = png_rgb(&[255, 0, 0, 0, 255, 0], 2, 1);
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(crc32(b"IEND", 0), 0xAE42_6082);
        #[cfg(feature = "images")]
        {
            let d = crate::image::decode_bytes(&png).unwrap();
            assert_eq!((d.w, d.h), (2, 1));
            assert_eq!(&d.rgb[..], &[255, 0, 0, 0, 255, 0]);
        }
        let mut k = Vec::new();
        kitty_transmit_rgb(&vec![7u8; 9000], 50, 60, 5, &mut k, false);
        let s = String::from_utf8(k).unwrap();
        assert_eq!(s.matches("\x1b_G").count(), 3);
        assert!(s.starts_with("\x1b_Ga=t,f=24,s=50,v=60,t=d,i=5,q=2,m=1;"));
        assert!(s.contains("\x1b_Gm=0;"));
        let mut t = Vec::new();
        wrap(b"\x1b_Ga=d\x1b\\", &mut t, true);
        assert_eq!(t, b"\x1bPtmux;\x1b\x1b_Ga=d\x1b\x1b\\\x1b\\".to_vec());
        let mut it = Vec::new();
        iterm_inline(b"abc", 3, 1, &mut it, false);
        assert_eq!(
            it,
            b"\x1b]1337;File=inline=1;size=3;width=3;height=1;preserveAspectRatio=0;doNotMoveCursor=1:YWJj\x07".to_vec()
        );
    }
}

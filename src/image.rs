//! Pictures: decoding (JPEG/PNG via zune), EXIF orientation, cover-crop resampling,
//! blur, palette extraction (median cut + k-means) and quantisation for sixel.
//! The decoded original is dropped as soon as it has been downscaled.

use crate::color::{chf, from_f, hsv_to_rgb, lum, mix, rgb, rgb_to_hsv, Rgb};

pub struct Img {
    pub w: usize,
    pub h: usize,
    pub rgb: Vec<u8>,
}

pub fn decode_file(path: &str) -> Result<Img, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    decode_bytes(&data)
}

#[cfg(not(feature = "images"))]
pub fn decode_bytes(_data: &[u8]) -> Result<Img, String> {
    Err("built without image support".into())
}

#[cfg(feature = "images")]
pub fn decode_bytes(data: &[u8]) -> Result<Img, String> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let limits = DecoderOptions::default()
        .set_max_width(20000)
        .set_max_height(20000);
    if data.starts_with(b"\x89PNG") {
        let mut d = zune_png::PngDecoder::new_with_options(
            ZCursor::new(data),
            limits.png_set_strip_to_8bit(true),
        );
        let px = d.decode_raw().map_err(|e| format!("PNG: {e:?}"))?;
        let (w, h) = d.dimensions().ok_or("PNG: no size")?;
        let cs = d.colorspace().unwrap_or(ColorSpace::RGB);
        Ok(Img {
            w,
            h,
            rgb: to_rgb(px, cs.num_components(), w * h),
        })
    } else if data.starts_with(&[0xff, 0xd8]) {
        let mut d = zune_jpeg::JpegDecoder::new_with_options(
            ZCursor::new(data),
            limits.jpeg_set_out_colorspace(ColorSpace::RGB),
        );
        let px = d.decode().map_err(|e| format!("JPEG: {e:?}"))?;
        let (w, h) = d.dimensions().ok_or("JPEG: no size")?;
        let n = d
            .output_colorspace()
            .map(|c| c.num_components())
            .unwrap_or(3);
        let orient = d.exif().map(|e| exif_orientation(e)).unwrap_or(1);
        let img = Img {
            w,
            h,
            rgb: to_rgb(px, n, w * h),
        };
        Ok(orient_img(img, orient))
    } else {
        Err(format!(
            "unsupported image format ({}): use JPEG or PNG",
            sniff(data)
        ))
    }
}

pub fn sniff(data: &[u8]) -> &'static str {
    if data.starts_with(b"GIF8") {
        "GIF"
    } else if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "WebP"
    } else if data.starts_with(b"<") || data.starts_with(b"\xef\xbb\xbf<") {
        "HTML/SVG/XML"
    } else if data.starts_with(b"PK") || data.starts_with(b"\x1f\x8b") {
        "archive"
    } else {
        "unknown"
    }
}

#[cfg(feature = "images")]
fn to_rgb(px: Vec<u8>, n: usize, count: usize) -> Vec<u8> {
    match n {
        3 => px,
        4 => {
            // flatten alpha over black
            let mut o = Vec::with_capacity(count * 3);
            for p in px.chunks_exact(4) {
                let a = p[3] as u32;
                o.extend_from_slice(&[
                    (p[0] as u32 * a / 255) as u8,
                    (p[1] as u32 * a / 255) as u8,
                    (p[2] as u32 * a / 255) as u8,
                ]);
            }
            o
        }
        1 | 2 => {
            let mut o = Vec::with_capacity(count * 3);
            for p in px.chunks_exact(n) {
                let v = if n == 2 {
                    (p[0] as u32 * p[1] as u32 / 255) as u8
                } else {
                    p[0]
                };
                o.extend_from_slice(&[v, v, v]);
            }
            o
        }
        _ => px,
    }
}

/// EXIF orientation tag (1..8) from raw EXIF/TIFF bytes.
pub fn exif_orientation(e: &[u8]) -> u8 {
    let t = if e.starts_with(b"Exif\0\0") {
        &e[6..]
    } else {
        e
    };
    if t.len() < 8 {
        return 1;
    }
    let le = &t[0..2] == b"II";
    let u16_at = |o: usize| -> Option<u16> {
        let b = t.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = t.get(o..o + 4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    };
    let Some(ifd) = u32_at(4) else { return 1 };
    let ifd = ifd as usize;
    let n = u16_at(ifd).unwrap_or(0) as usize;
    for k in 0..n.min(64) {
        let o = ifd + 2 + k * 12;
        if u16_at(o) == Some(0x0112) {
            return u16_at(o + 8).unwrap_or(1).clamp(1, 8) as u8;
        }
    }
    1
}

fn orient_img(img: Img, o: u8) -> Img {
    if o <= 1 {
        return img;
    }
    let (w, h) = (img.w, img.h);
    let swap = o >= 5;
    let (nw, nh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; nw * nh * 3];
    for y in 0..nh {
        for x in 0..nw {
            // source coordinate for destination (x, y)
            let (sx, sy) = match o {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                8 => (w - 1 - y, x),
                _ => (x, y),
            };
            let s = (sy * w + sx) * 3;
            let d = (y * nw + x) * 3;
            out[d..d + 3].copy_from_slice(&img.rgb[s..s + 3]);
        }
    }
    Img {
        w: nw,
        h: nh,
        rgb: out,
    }
}

/// Cover-crop (centre) and resample to dw × dh: area average when shrinking,
/// bilinear when enlarging.
pub fn cover(src: &Img, dw: usize, dh: usize) -> Vec<u8> {
    let (sw, sh) = (src.w as f64, src.h as f64);
    let scale = (dw as f64 / sw).max(dh as f64 / sh);
    let (cw, chh) = (dw as f64 / scale, dh as f64 / scale);
    let (x0, y0) = ((sw - cw) / 2.0, (sh - chh) / 2.0);
    resample(src, (x0, y0, cw, chh), dw, dh)
}

/// Stretch the whole image to dw × dh.
pub fn stretch(src: &Img, dw: usize, dh: usize) -> Vec<u8> {
    resample(src, (0.0, 0.0, src.w as f64, src.h as f64), dw, dh)
}

fn resample(src: &Img, crop: (f64, f64, f64, f64), dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 3];
    if dw == 0 || dh == 0 || src.w == 0 || src.h == 0 {
        return out;
    }
    let (x0, y0, cw, chh) = crop;
    let (fx, fy) = (cw / dw as f64, chh / dh as f64);
    let s = &src.rgb;
    let sw = src.w;
    if fx >= 1.0 && fy >= 1.0 {
        // area average: per output column, the source span with fractional edge weights
        let spans = |o: f64, f: f64, n: usize, lim: usize| -> Vec<(usize, Vec<f32>)> {
            (0..n)
                .map(|i| {
                    let a = o + i as f64 * f;
                    let b = a + f;
                    let st = a.floor().max(0.0) as usize;
                    let en = (b.ceil() as usize).min(lim);
                    let ws = (st..en)
                        .map(|k| ((k as f64 + 1.0).min(b) - (k as f64).max(a)).max(0.0) as f32)
                        .collect();
                    (st, ws)
                })
                .collect()
        };
        let xs = spans(x0, fx, dw, src.w);
        let ys = spans(y0, fy, dh, src.h);
        let mut acc = vec![[0f32; 3]; dw];
        for (oy, (ys0, yw)) in ys.iter().enumerate() {
            acc.iter_mut().for_each(|a| *a = [0.0; 3]);
            let mut wsum_y = 0.0;
            for (j, &wy) in yw.iter().enumerate() {
                let row = &s[(ys0 + j) * sw * 3..];
                wsum_y += wy;
                for (ox, (xs0, xw)) in xs.iter().enumerate() {
                    let mut c = [0f32; 3];
                    for (i, &wx) in xw.iter().enumerate() {
                        let p = (xs0 + i) * 3;
                        c[0] += row[p] as f32 * wx;
                        c[1] += row[p + 1] as f32 * wx;
                        c[2] += row[p + 2] as f32 * wx;
                    }
                    let a = &mut acc[ox];
                    a[0] += c[0] * wy;
                    a[1] += c[1] * wy;
                    a[2] += c[2] * wy;
                }
            }
            for (ox, (_, xw)) in xs.iter().enumerate() {
                let wsum: f32 = xw.iter().sum::<f32>() * wsum_y;
                let d = (oy * dw + ox) * 3;
                for k in 0..3 {
                    out[d + k] = (acc[ox][k] / wsum.max(1e-6) + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        }
    } else {
        for oy in 0..dh {
            let sy = (y0 + (oy as f64 + 0.5) * fy - 0.5)
                .max(0.0)
                .min(src.h as f64 - 1.0);
            let (iy, ty) = (sy.floor() as usize, (sy - sy.floor()) as f32);
            let iy1 = (iy + 1).min(src.h - 1);
            for ox in 0..dw {
                let sx = (x0 + (ox as f64 + 0.5) * fx - 0.5)
                    .max(0.0)
                    .min(src.w as f64 - 1.0);
                let (ix, tx) = (sx.floor() as usize, (sx - sx.floor()) as f32);
                let ix1 = (ix + 1).min(src.w - 1);
                let d = (oy * dw + ox) * 3;
                for k in 0..3 {
                    let p = |x: usize, y: usize| s[(y * sw + x) * 3 + k] as f32;
                    let top = p(ix, iy) + (p(ix1, iy) - p(ix, iy)) * tx;
                    let bot = p(ix, iy1) + (p(ix1, iy1) - p(ix, iy1)) * tx;
                    out[d + k] = (top + (bot - top) * ty + 0.5) as u8;
                }
            }
        }
    }
    out
}

/// Approximate Gaussian blur: three box-blur passes, radius in pixels (x and y).
pub fn blur(rgb: &mut [u8], w: usize, h: usize, rx: f32, ry: f32) {
    let pass = |rgb: &mut [u8], len: usize, count: usize, stride: usize, step: usize, r: usize| {
        if r == 0 {
            return;
        }
        let mut line = vec![[0u32; 3]; len];
        for c in 0..count {
            let base = c * stride;
            for (i, l) in line.iter_mut().enumerate() {
                let p = base + i * step;
                *l = [rgb[p] as u32, rgb[p + 1] as u32, rgb[p + 2] as u32];
            }
            let win = (2 * r + 1) as u32;
            let mut sum = [0u32; 3];
            let at = |i: isize| line[i.clamp(0, len as isize - 1) as usize];
            for i in -(r as isize)..=(r as isize) {
                let v = at(i);
                for k in 0..3 {
                    sum[k] += v[k];
                }
            }
            for i in 0..len {
                let p = base + i * step;
                for k in 0..3 {
                    rgb[p + k] = (sum[k] / win) as u8;
                }
                let add = at(i as isize + r as isize + 1);
                let sub = at(i as isize - r as isize);
                for k in 0..3 {
                    sum[k] = sum[k] + add[k] - sub[k];
                }
            }
        }
    };
    // three passes of radius r/√3 ≈ a Gaussian of sigma r/… good enough for backgrounds
    let bx = ((rx / 1.73).round() as usize).max(if rx > 0.0 { 1 } else { 0 });
    let by = ((ry / 1.73).round() as usize).max(if ry > 0.0 { 1 } else { 0 });
    for _ in 0..3 {
        pass(rgb, w, h, w * 3, 3, bx);
        pass(rgb, h, w, 3, w * 3, by);
    }
}

// ── colour quantisation ──────────────────────────────────────────────────────

/// Median cut on a list of colours, then `iters` k-means passes. Returns (colours, counts).
pub fn median_cut(px: &[[u8; 3]], k: usize, iters: usize) -> (Vec<[f32; 3]>, Vec<usize>) {
    if px.is_empty() {
        return (vec![[0.0; 3]], vec![1]);
    }
    let mut boxes: Vec<Vec<[u8; 3]>> = vec![px.to_vec()];
    while boxes.len() < k {
        // split the box with the largest channel range (weighted by population)
        let mut best = None;
        for (bi, b) in boxes.iter().enumerate() {
            if b.len() < 2 {
                continue;
            }
            for c in 0..3 {
                let (mn, mx) = b
                    .iter()
                    .fold((255u8, 0u8), |(a, z), p| (a.min(p[c]), z.max(p[c])));
                let score = (mx - mn) as usize * (b.len() as f64).sqrt() as usize;
                if mx > mn && best.is_none_or(|(s, _, _)| score > s) {
                    best = Some((score, bi, c));
                }
            }
        }
        let Some((_, bi, c)) = best else { break };
        let mut b = boxes.swap_remove(bi);
        b.sort_unstable_by_key(|p| p[c]);
        let hi = b.split_off(b.len() / 2);
        boxes.push(b);
        boxes.push(hi);
    }
    let mut cent: Vec<[f32; 3]> = boxes
        .iter()
        .map(|b| {
            let mut s = [0f64; 3];
            for p in b {
                for c in 0..3 {
                    s[c] += p[c] as f64;
                }
            }
            let n = b.len().max(1) as f64;
            [(s[0] / n) as f32, (s[1] / n) as f32, (s[2] / n) as f32]
        })
        .collect();
    let mut counts = vec![0usize; cent.len()];
    for it in 0..=iters {
        let mut sums = vec![[0f64; 4]; cent.len()];
        counts.iter_mut().for_each(|c| *c = 0);
        for p in px {
            let i = nearest_f(&cent, p);
            counts[i] += 1;
            let s = &mut sums[i];
            s[0] += p[0] as f64;
            s[1] += p[1] as f64;
            s[2] += p[2] as f64;
            s[3] += 1.0;
        }
        if it == iters {
            break;
        }
        for (c, s) in cent.iter_mut().zip(&sums) {
            if s[3] > 0.0 {
                *c = [
                    (s[0] / s[3]) as f32,
                    (s[1] / s[3]) as f32,
                    (s[2] / s[3]) as f32,
                ];
            }
        }
    }
    (cent, counts)
}

fn nearest_f(cent: &[[f32; 3]], p: &[u8; 3]) -> usize {
    let mut best = (f32::MAX, 0);
    for (i, c) in cent.iter().enumerate() {
        let d = (c[0] - p[0] as f32).powi(2)
            + (c[1] - p[1] as f32).powi(2)
            + (c[2] - p[2] as f32).powi(2);
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1
}

/// An image quantised to a palette (for sixel patches): indices + colours.
pub struct Indexed {
    pub w: usize,
    pub h: usize,
    pub idx: Vec<u8>,
    pub pal: Vec<Rgb>,
}

/// Quantise to `k` colours (median cut on a subsample + k-means) with Floyd–Steinberg.
pub fn quantize(rgb: &[u8], w: usize, h: usize, k: usize) -> Indexed {
    let n = w * h;
    let step = (n / 40000).max(1);
    let samples: Vec<[u8; 3]> = (0..n)
        .step_by(step)
        .map(|i| [rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]])
        .collect();
    let (cent, _) = median_cut(&samples, k, 2);
    let pal: Vec<Rgb> = cent.iter().map(|c| rgb_f255(*c)).collect();
    let pc: Vec<[i32; 3]> = pal
        .iter()
        .map(|&c| {
            [
                (c >> 16) as i32 & 255,
                (c >> 8) as i32 & 255,
                c as i32 & 255,
            ]
        })
        .collect();
    // 15-bit lookup table: nearest palette entry for each 5-bit-per-channel colour
    let mut lut = vec![0u8; 32768];
    for (q, l) in lut.iter_mut().enumerate() {
        let (r, g, b) = (
            ((q >> 10) & 31) as i32 * 8 + 4,
            ((q >> 5) & 31) as i32 * 8 + 4,
            (q & 31) as i32 * 8 + 4,
        );
        let mut best = (i32::MAX, 0usize);
        for (i, c) in pc.iter().enumerate() {
            let d = (c[0] - r).pow(2) + (c[1] - g).pow(2) + (c[2] - b).pow(2);
            if d < best.0 {
                best = (d, i);
            }
        }
        *l = best.1 as u8;
    }
    let mut idx = vec![0u8; n];
    let mut err = vec![[0i32; 3]; (w + 2) * 2];
    for y in 0..h {
        let (cur, nxt) = if y % 2 == 0 { (0, w + 2) } else { (w + 2, 0) };
        for e in &mut err[nxt..nxt + w + 2] {
            *e = [0; 3];
        }
        for x in 0..w {
            let p = (y * w + x) * 3;
            let e = err[cur + x + 1];
            let c = [
                (rgb[p] as i32 + e[0] / 16).clamp(0, 255),
                (rgb[p + 1] as i32 + e[1] / 16).clamp(0, 255),
                (rgb[p + 2] as i32 + e[2] / 16).clamp(0, 255),
            ];
            let i = lut[((c[0] >> 3) << 10 | (c[1] >> 3) << 5 | (c[2] >> 3)) as usize];
            idx[y * w + x] = i;
            let q = pc[i as usize];
            let d = [c[0] - q[0], c[1] - q[1], c[2] - q[2]];
            for k in 0..3 {
                err[cur + x + 2][k] += d[k] * 7;
                err[nxt + x][k] += d[k] * 3;
                err[nxt + x + 1][k] += d[k] * 5;
                err[nxt + x + 2][k] += d[k];
            }
        }
    }
    Indexed { w, h, idx, pal }
}

fn rgb_f255(c: [f32; 3]) -> Rgb {
    let q = |v: f32| (v + 0.5).clamp(0.0, 255.0) as u8;
    rgb(q(c[0]), q(c[1]), q(c[2]))
}

// ── palette from an image (themes/README.md "Image themes") ──────────────────

pub struct ImgPalette {
    pub tint: Rgb,
    pub a1: Rgb,
    pub a2: Rgb,
    pub text: Rgb,
}

pub fn palette_from(img: &Img) -> ImgPalette {
    let thumb = stretch(img, 96, 96);
    let px: Vec<[u8; 3]> = thumb.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect();
    let (cent, counts) = median_cut(&px, 8, 3);
    let total = counts.iter().sum::<usize>().max(1) as f32;
    let cols: Vec<([f32; 3], f32)> = cent
        .iter()
        .zip(&counts)
        .map(|(c, &n)| ([c[0] / 255.0, c[1] / 255.0, c[2] / 255.0], n as f32 / total))
        .collect();
    let base = cols
        .iter()
        .filter(|(_, s)| *s > 0.05)
        .min_by(|a, b| lum(from_f(a.0)).total_cmp(&lum(from_f(b.0))))
        .map(|c| c.0)
        .unwrap_or(cols[0].0);
    let vivid_score = |c: &([f32; 3], f32)| {
        let h = rgb_to_hsv(c.0);
        h[1] * h[2] * c.1.sqrt()
    };
    let mut vivid = cols.clone();
    crate::sys::isort(&mut vivid, &|a, b| vivid_score(a) > vivid_score(b));
    let acc1 = vivid[0].0;
    let h1 = rgb_to_hsv(acc1)[0];
    let acc2 = vivid[1..]
        .iter()
        .find(|(c, _)| {
            let d = (rgb_to_hsv(*c)[0] - h1).abs();
            d.min(1.0 - d) > 40.0 / 360.0
        })
        .map(|c| c.0)
        .unwrap_or(if vivid.len() > 1 { vivid[1].0 } else { acc1 });
    let with = |c: [f32; 3], s: Option<f32>, v: Option<f32>| {
        let mut h = rgb_to_hsv(c);
        if let Some(s) = s {
            h[1] = s;
        }
        if let Some(v) = v {
            h[2] = v;
        }
        hsv_to_rgb(h)
    };
    let a1 = with(
        acc1,
        Some(rgb_to_hsv(acc1)[1].clamp(0.45, 0.75)),
        Some(0.95),
    );
    let a2 = with(acc2, Some(rgb_to_hsv(acc2)[1].clamp(0.4, 0.7)), Some(0.9));
    let mut tint = base;
    let mut guard = 0;
    while lum(from_f(tint)) > 0.012 && guard < 200 {
        tint = [tint[0] * 0.85, tint[1] * 0.85, tint[2] * 0.85];
        guard += 1;
    }
    let (a1, a2, tint) = (from_f(a1), from_f(a2), from_f(tint));
    let text = mix(from_f([0.93, 0.94, 0.97]), a1, 0.12);
    ImgPalette { tint, a1, a2, text }
}

/// Value-adjusted copy (freq ramp start: accent2 at value 0.45).
pub fn with_value(c: Rgb, v: f32) -> Rgb {
    let mut h = rgb_to_hsv(chf(c));
    h[2] = v;
    from_f(hsv_to_rgb(h))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resample_and_quantize() {
        let img = Img {
            w: 4,
            h: 2,
            rgb: [[255u8, 0, 0]; 4]
                .iter()
                .chain([[0u8, 0, 255]; 4].iter())
                .flatten()
                .copied()
                .collect(),
        };
        let o = stretch(&img, 2, 1);
        assert_eq!(o, vec![128, 0, 128, 128, 0, 128]);
        let c = cover(&img, 1, 1); // centre crop 2x2 → average
        assert_eq!(c, vec![128, 0, 128]);
        let up = stretch(&img, 8, 4);
        assert_eq!(&up[0..3], &[255, 0, 0]);
        let q = quantize(&img.rgb, 4, 2, 2);
        assert_eq!(q.pal.len(), 2);
        assert_ne!(q.idx[0], q.idx[7]);
    }
    #[test]
    fn exif() {
        // II*\0, IFD at 8, 1 entry: tag 0x0112 type 3 count 1 value 6
        let e = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0";
        assert_eq!(exif_orientation(e), 6);
        let img = Img {
            w: 2,
            h: 1,
            rgb: vec![1, 1, 1, 2, 2, 2],
        };
        let r = orient_img(img, 6);
        assert_eq!((r.w, r.h), (1, 2));
        assert_eq!(r.rgb, vec![1, 1, 1, 2, 2, 2]);
    }
}

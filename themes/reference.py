#!/usr/bin/env python3
"""Reference implementation of cpuview themes (spec: themes/README.md).

This file is the executable specification for the theme data in themes/*.toml:
  * colour helpers and gradient ramps
  * the deterministic hash / value-noise / fBm used by every procedural layer
  * every background layer type (gradient, radial, noise, stars, aurora, sun,
    grid, silhouette, rays, vignette, dotgrid)
  * the cell compositor that blends dashboard cells over a background with
    half-blocks ("blocks" image mode)
  * palette extraction for image themes (palette_from_image)

It is deliberately plain Python (Pillow only for PNG I/O and image themes), so it can
be read side by side with a port. Run `python3 reference.py --help`.
"""
from __future__ import annotations

import argparse
import colorsys
import math
import os
import sys
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))

# ── colour ────────────────────────────────────────────────────────────────────


def hex2rgb(h):
    h = h.lstrip("#")
    return tuple(int(h[i:i + 2], 16) / 255 for i in (0, 2, 4))


def rgb2hex(c):
    return "#%02x%02x%02x" % tuple(max(0, min(255, round(v * 255))) for v in c)


def lerp(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def clamp01(x):
    return 0.0 if x < 0 else 1.0 if x > 1 else x


def ramp(stops):
    """Gradient through colour stops. `stops` is a list of "#rrggbb" (evenly spaced)
    or of [position, "#rrggbb"] pairs. Returns f(t) -> rgb for t in [0, 1]."""
    if isinstance(stops[0], str):
        n = len(stops) - 1
        pts = [(i / n if n else 0.0, hex2rgb(s)) for i, s in enumerate(stops)]
    else:
        pts = [(float(p), hex2rgb(s)) for p, s in stops]

    def f(t):
        t = clamp01(t)
        if t <= pts[0][0]:
            return pts[0][1]
        for (p0, c0), (p1, c1) in zip(pts, pts[1:]):
            if t <= p1:
                return lerp(c0, c1, (t - p0) / (p1 - p0) if p1 > p0 else 0.0)
        return pts[-1][1]
    return f


def luminance(c):
    """Relative luminance (sRGB, WCAG)."""
    def lin(v):
        return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4
    r, g, b = (lin(v) for v in c)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(a, b):
    la, lb = sorted((luminance(a), luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


# ── deterministic noise ───────────────────────────────────────────────────────
# All arithmetic is on unsigned 32-bit integers so a port reproduces the exact
# same backgrounds (wrap on overflow: Rust `u32::wrapping_mul`).

M32 = 0xFFFFFFFF


def hash_u32(ix, iy, seed):
    h = (ix * 374761393 + iy * 668265263 + seed * 2246822519) & M32
    h = ((h ^ (h >> 13)) * 1274126177) & M32
    return h ^ (h >> 16)


def hash01(ix, iy, seed):
    """Uniform in [0, 1]."""
    return hash_u32(ix, iy, seed) / M32


def fade(t):  # quintic smoothstep
    return t * t * t * (t * (t * 6 - 15) + 10)


def value_noise(x, y, seed):
    """Smooth value noise in [0, 1]: random values on the integer lattice,
    quintic-interpolated."""
    ix, iy = math.floor(x), math.floor(y)
    fx, fy = fade(x - ix), fade(y - iy)
    a = hash01(ix, iy, seed)
    b = hash01(ix + 1, iy, seed)
    c = hash01(ix, iy + 1, seed)
    d = hash01(ix + 1, iy + 1, seed)
    top = a + (b - a) * fx
    bot = c + (d - c) * fx
    return top + (bot - top) * fy


def fbm(x, y, seed, octaves=5, lacunarity=2.0, gain=0.5, ridged=False):
    """Fractal sum of value noise, normalised to [0, 1]. Octave k uses seed + k.
    ridged: each octave contributes 1 - |2n - 1| (sharp crests, for caustics/dust)."""
    amp, freq, total, norm = 1.0, 1.0, 0.0, 0.0
    for k in range(octaves):
        n = value_noise(x * freq, y * freq, seed + k)
        if ridged:
            n = 1.0 - abs(2.0 * n - 1.0)
        total += amp * n
        norm += amp
        amp *= gain
        freq *= lacunarity
    return total / norm


def contrast_curve(v, threshold=0.0, power=1.0):
    """Map noise to a mask: 0 below `threshold`, rising to 1, shaped by `power`."""
    v = clamp01((v - threshold) / (1.0 - threshold)) if threshold < 1 else 0.0
    return v ** power


# ── background canvas ─────────────────────────────────────────────────────────
# The background is rendered at W x H "pixels" where W = terminal columns and
# H = 2 * terminal rows (one cell = two vertically stacked half-block pixels).
# Coordinates given to layers:  u = (x + 0.5) / W, v = (y + 0.5) / H  (0..1),
# and  s = (x + 0.5) / H  — a horizontal coordinate in units of the height, so
# shapes (sun discs, noise features) stay round whatever the aspect ratio.
# Half-block pixels are treated as square (true for typical 1:2 terminal cells).


class Canvas:
    def __init__(self, w, h, hires=False):
        self.w, self.h = w, h
        self.hires = hires   # True: one canvas pixel = one screen pixel (sixel/kitty/iterm)
        self.px = [[(0.0, 0.0, 0.0)] * w for _ in range(h)]
        self.glyphs = {}   # (col, row) of a *cell* -> (char, rgb): stars drawn as text

    def coords(self):
        W, H = self.w, self.h
        for y in range(H):
            v = (y + 0.5) / H
            for x in range(W):
                yield x, y, (x + 0.5) / W, v, (x + 0.5) / H


def blend(dst, src, alpha, mode="over"):
    """Blend `src` onto `dst` with opacity `alpha`. Modes: over, add, screen, multiply."""
    if alpha <= 0:
        return dst
    if mode == "add":
        out = tuple(min(1.0, d + s * alpha) for d, s in zip(dst, src))
    elif mode == "screen":
        out = tuple(d + (1 - d) * s * alpha for d, s in zip(dst, src))
    elif mode == "multiply":
        out = tuple(d * (1 - alpha + s * alpha) for d, s in zip(dst, src))
    else:
        out = tuple(d + (s - d) * alpha for d, s in zip(dst, src))
    return out


# ── layers ────────────────────────────────────────────────────────────────────


def layer_gradient(cv, p):
    """Linear gradient. angle: degrees, 0 = top->bottom, 90 = left->right."""
    f = ramp(p["stops"])
    a = math.radians(p.get("angle", 0.0))
    dx, dy = math.sin(a), math.cos(a)
    # project (u, v) on the direction and normalise to 0..1 over the canvas
    corners = [u * dx + v * dy for u in (0, 1) for v in (0, 1)]
    lo, hi = min(corners), max(corners)
    mode, op = p.get("blend", "over"), p.get("opacity", 1.0)
    for x, y, u, v, s in cv.coords():
        t = (u * dx + v * dy - lo) / (hi - lo)
        cv.px[y][x] = blend(cv.px[y][x], f(t), op, mode)


def layer_radial(cv, p):
    """Radial glow. center: [u, v]; radius in units of height; stops from centre outwards."""
    f = ramp(p["stops"])
    cu, cvv = p.get("center", [0.5, 0.5])
    r = p.get("radius", 0.5)
    mode, op = p.get("blend", "screen"), p.get("opacity", 1.0)
    cx = cu * cv.w / cv.h
    for x, y, u, v, s in cv.coords():
        d = math.hypot(s - cx, v - cvv) / r
        if d < 1.0:
            # fade the glow out towards the rim
            cv.px[y][x] = blend(cv.px[y][x], f(d), op * (1 - d) ** p.get("falloff", 1.5), mode)


def band_mask(p, u, v):
    """Optional diagonal band restricting a layer: band = {center, slope, width}.
    Mask = exp(-((v - (center + slope*(u - 0.5))) / width)^2)."""
    b = p.get("band")
    if not b:
        return 1.0
    d = (v - (b["center"] + b.get("slope", 0.0) * (u - 0.5))) / b["width"]
    return math.exp(-d * d)


def layer_noise(cv, p):
    """fBm clouds. The noise value n is mapped through contrast_curve(threshold, power)
    to a mask m, coloured with ramp(colors)(m) and blended with opacity * m.
    warp > 0 applies one round of domain warping (offset by two more fBm lookups)."""
    seed = p.get("seed", 1)
    sc = p.get("scale", 3.0)
    oc, lac, gain = p.get("octaves", 5), p.get("lacunarity", 2.0), p.get("gain", 0.5)
    ridged = p.get("ridged", False)
    warp = p.get("warp", 0.0)
    stretch = p.get("stretch", [1.0, 1.0])     # anisotropic scale (x, y)
    f = ramp(p["colors"])
    mode, op = p.get("blend", "screen"), p.get("opacity", 1.0)
    thr, pw = p.get("threshold", 0.0), p.get("power", 1.0)
    for x, y, u, v, s in cv.coords():
        nx, ny = s * sc * stretch[0], v * sc * stretch[1]
        if warp:
            wx = fbm(nx + 5.2, ny + 1.3, seed + 101, 3)
            wy = fbm(nx + 1.7, ny + 9.2, seed + 202, 3)
            nx, ny = nx + warp * (wx - 0.5) * 2, ny + warp * (wy - 0.5) * 2
        n = fbm(nx, ny, seed, oc, lac, gain, ridged)
        m = contrast_curve(n, thr, pw) * band_mask(p, u, v)
        if m > 0:
            cv.px[y][x] = blend(cv.px[y][x], f(m), op * m, mode)


def layer_stars(cv, p):
    """Starfield.
    Pixel stars: a pixel is a star when hash01(x, y, seed) < density * dens(v).
      brightness b = min_b + (1 - min_b) * hash01(x, y, seed + 1) ** 3 (few bright, many faint)
      colour      = colors[floor(hash01(x, y, seed + 2) * len(colors))]
      added (blend add) at alpha b.
    Glyph stars (drawn as text in blank cells, sharper than a half-block pixel):
      a cell (cx, cy) gets one when hash01(cx, cy, seed + 3) < glyph_density * dens(v);
      glyph = glyphs[floor(hash01(cx, cy, seed + 4) ** 2 * len(glyphs))]  (small glyphs commoner)
      colour = colour of the same palette * brightness (as above, with seed + 5).
    dens(v) = 1 + density_gradient * (v - 0.5) * 2   (positive: more stars lower down).
    Pixel stars are only drawn when the canvas is at real pixel resolution (hi-res image
    modes). At half-block resolution a one-pixel star is a whole half cell and looks like
    a rendering glitch, so cell renders (blocks mode) use the glyph stars alone."""
    seed = p.get("seed", 7)
    dens = p.get("density", 0.004)
    grad = p.get("density_gradient", 0.0)
    min_b = p.get("min_brightness", 0.25)
    cols = [hex2rgb(c) for c in p.get("colors", ["#ffffff"])]
    for x, y, u, v, s in cv.coords() if cv.hires else ():
        d = dens * max(0.0, 1 + grad * (v - 0.5) * 2)
        if hash01(x, y, seed) < d:
            b = min_b + (1 - min_b) * hash01(x, y, seed + 1) ** 3
            c = cols[int(hash01(x, y, seed + 2) * len(cols)) % len(cols)]
            cv.px[y][x] = blend(cv.px[y][x], c, b, "add")
    gd = p.get("glyph_density", 0.0)
    glyphs = p.get("glyphs", ["·", "·", "∙", "•"])
    if gd:
        rows = cv.h // 2
        for cy in range(rows):
            v = (cy + 0.5) / rows
            d = gd * max(0.0, 1 + grad * (v - 0.5) * 2)
            for cx in range(cv.w):
                if hash01(cx, cy, seed + 3) < d:
                    g = glyphs[int(hash01(cx, cy, seed + 4) ** 2 * len(glyphs)) % len(glyphs)]
                    b = min_b + (1 - min_b) * hash01(cx, cy, seed + 5) ** 2
                    c = cols[int(hash01(cx, cy, seed + 6) * len(cols)) % len(cols)]
                    cv.glyphs[(cx, cy)] = (g, tuple(ch * b for ch in c))


def layer_aurora(cv, p):
    """Aurora curtains. For each band:
      centre line  yc(u) = y + amplitude * sin(2π(u * waves + phase))
                           + amplitude * (fbm(u * 3, seed) - 0.5)
      rays         r(u)  = 0.55 + 0.45 * value_noise(u * streaks, 0.5, seed + 9)
      above the line (d = yc - v > 0): intensity = r * exp(-d / height)
      below the line (d < 0):          intensity = exp(d / (height * 0.15))   (sharp lower edge)
      colour = ramp([bottom_color, top_color])(clamp(d / height))
    blended with mode `add` at opacity * intensity."""
    seed = p.get("seed", 3)
    op = p.get("opacity", 1.0)
    for i, b in enumerate(p["bands"]):
        f = ramp([b["bottom_color"], b["top_color"]])
        y0, amp = b["y"], b.get("amplitude", 0.06)
        waves, phase = b.get("waves", 1.5), b.get("phase", 0.0)
        h = b.get("height", 0.18)
        streaks = b.get("streaks", 90)
        sd = seed + 31 * i
        line = [y0 + amp * math.sin(2 * math.pi * ((x + 0.5) / cv.w * waves + phase))
                + amp * (fbm((x + 0.5) / cv.w * 3, 0.0, sd, 3) - 0.5) for x in range(cv.w)]
        rays = [0.55 + 0.45 * value_noise((x + 0.5) / cv.w * streaks, 0.5, sd + 9) for x in range(cv.w)]
        for x, y, u, v, s in cv.coords():
            d = line[x] - v
            if d > 0:
                k = rays[x] * math.exp(-d / h)
            else:
                k = math.exp(d / (h * 0.15))
            if k > 0.004:
                cv.px[y][x] = blend(cv.px[y][x], f(clamp01(d / h)), op * k * b.get("intensity", 1.0), "add")


def layer_sun(cv, p):
    """Retro sun: disc of radius r (height units) at center [u, v], vertical colour
    ramp top->bottom, with horizontal gaps ("stripes") in its lower half that get
    thicker towards the bottom, plus a soft glow (radial, add). Pixels with
    v > clip_below (the horizon) are left untouched."""
    cu, cvv = p.get("center", [0.5, 0.5])
    r = p.get("radius", 0.2)
    f = ramp(p["stops"])
    cx = cu * cv.w / cv.h
    stripes = p.get("stripes", 6)
    glow = hex2rgb(p.get("glow", p["stops"][-1]))
    clip = p.get("clip_below", 2.0)
    for x, y, u, v, s in cv.coords():
        if v > clip:
            continue
        d = math.hypot(s - cx, v - cvv)
        t = (v - (cvv - r)) / (2 * r)          # 0 at top of disc, 1 at bottom
        if d < r:
            gap = False
            if stripes and t > 0.5:
                k = (t - 0.5) * 2                  # 0..1 over the lower half
                period = 1.0 / stripes
                phase = (k % period) / period
                gap = phase < 0.15 + 0.55 * k
            if not gap:
                cv.px[y][x] = f(t)
                continue
        # glow outside the disc (and in the stripe gaps)
        g = max(0.0, 1 - (d - r) / (r * p.get("glow_size", 1.2))) if d >= r else 0.35
        if g > 0:
            cv.px[y][x] = blend(cv.px[y][x], glow, p.get("glow_opacity", 0.35) * g * g, "add")


def layer_grid(cv, p):
    """Synthwave perspective grid below `horizon` (v). For a pixel below the horizon,
    depth z = depth_scale / (v - horizon). Horizontal lines where frac(z * density) is
    near 0, vertical lines where frac((u - 0.5) / (v - horizon) * columns * 0.1) is near 0. Line weight
    is an anti-aliased band of width `line` (in fractional units) and fades toward the
    horizon with fade = clamp((v - horizon) / fade_distance)."""
    hz = p.get("horizon", 0.6)
    col = hex2rgb(p.get("color", "#ff3ec8"))
    dens = p.get("density", 1.0)
    ncol = p.get("columns", 10)
    lw = p.get("line", 0.06)
    zs = p.get("depth_scale", 0.12)
    fd = p.get("fade_distance", 0.25)
    op = p.get("opacity", 1.0)
    for x, y, u, v, s in cv.coords():
        if v <= hz:
            continue
        dz = v - hz
        z = zs / dz
        fz = (z * dens) % 1.0
        fx = ((u - 0.5) / dz * ncol * 0.1) % 1.0
        hline = max(0.0, 1 - min(fz, 1 - fz) / lw)
        vline = max(0.0, 1 - min(fx, 1 - fx) / (lw * 1.6))
        k = max(hline, vline) * clamp01(dz / fd)
        if k > 0:
            cv.px[y][x] = blend(cv.px[y][x], col, op * k, "add")


def layer_silhouette(cv, p):
    """Filled skyline. kind = "mountains": ridge v(u) = base - height * fbm(u * roughness, seed)
    kind = "trees": ridge from a low mountain line plus conifers: each column bucket of
    width `tree_width` (in columns) gets a tree of height tree_height * (0.5 + hash) whose
    outline is a triangle (|dx| / half_width shrinking upward) with a jagged edge.
    Pixels below the ridge take `color`; the `rim` colour (if any) is added on the top
    pixel of the ridge for a lit edge."""
    kind = p.get("kind", "mountains")
    seed = p.get("seed", 5)
    base, hgt = p.get("base", 0.85), p.get("height", 0.15)
    rough = p.get("roughness", 4.0)
    col = hex2rgb(p["color"])
    op = p.get("opacity", 1.0)
    rim = hex2rgb(p["rim"]) if p.get("rim") else None
    ridge = []
    for x in range(cv.w):
        u = (x + 0.5) / cv.w
        r = base - hgt * fbm(u * rough, 0.0, seed, 5)
        if kind == "trees":
            tw = p.get("tree_width", 6)
            th = p.get("tree_height", 0.12)
            r = base - hgt * 0.3 * fbm(u * rough, 0.0, seed, 3)
            for b in (x // tw - 1, x // tw, x // tw + 1):
                if hash01(b, 0, seed + 11) < p.get("tree_fill", 0.85):
                    centre = (b + hash01(b, 1, seed + 11)) * tw
                    h = th * (0.5 + hash01(b, 2, seed + 11))
                    half = tw * 0.75 * (0.7 + 0.6 * hash01(b, 3, seed + 11))
                    dx = abs(x + 0.5 - centre) / half
                    if dx < 1:
                        jag = 0.08 * hash01(x, 7, seed + 13)
                        r = min(r, base - h * (1 - dx) + h * jag)
        ridge.append(r)
    for x, y, u, v, s in cv.coords():
        if v >= ridge[x]:
            c = col
            if rim and v - ridge[x] < 1.0 / cv.h:
                c = blend(col, rim, 0.6, "add")
            cv.px[y][x] = blend(cv.px[y][x], c, op, "over")


def layer_rays(cv, p):
    """God rays from a point above the canvas (center [u, v], v usually < 0):
    intensity = value_noise(angle * count, 0, seed) ** sharpness * exp(-dist / reach)."""
    cu, cvv = p.get("center", [0.5, -0.2])
    cx = cu * cv.w / cv.h
    col = hex2rgb(p.get("color", "#9fe8ff"))
    cnt, sharp, reach = p.get("count", 14), p.get("sharpness", 3.0), p.get("reach", 0.7)
    seed, op = p.get("seed", 4), p.get("opacity", 0.25)
    for x, y, u, v, s in cv.coords():
        ang = math.atan2(s - cx, v - cvv)
        dist = math.hypot(s - cx, v - cvv)
        k = value_noise(ang * cnt, 0.0, seed) ** sharp * math.exp(-dist / reach)
        cv.px[y][x] = blend(cv.px[y][x], col, op * k, "add")


def layer_vignette(cv, p):
    """Darken towards the corners: multiply by 1 - strength * r^2,
    r = distance from centre with u, v in -1..1 (corner r^2 = 2, so clamp)."""
    st = p.get("strength", 0.3)
    for x, y, u, v, s in cv.coords():
        r2 = ((u - 0.5) * 2) ** 2 + ((v - 0.5) * 2) ** 2
        k = max(0.0, 1 - st * r2 / 2)
        c = cv.px[y][x]
        cv.px[y][x] = (c[0] * k, c[1] * k, c[2] * k)


def layer_dotgrid(cv, p):
    """Glyph dots every `every` = [cols, rows] cells (text layer, like glyph stars)."""
    ec, er = p.get("every", [4, 2])
    g = p.get("glyph", "·")
    c = hex2rgb(p.get("color", "#262626"))
    for cy in range(cv.h // 2):
        for cx in range(cv.w):
            if cx % ec == ec // 2 and cy % er == 0:
                cv.glyphs[(cx, cy)] = (g, c)


LAYERS = {
    "gradient": layer_gradient, "radial": layer_radial, "noise": layer_noise,
    "stars": layer_stars, "aurora": layer_aurora, "sun": layer_sun, "grid": layer_grid,
    "silhouette": layer_silhouette, "rays": layer_rays, "vignette": layer_vignette,
    "dotgrid": layer_dotgrid,
}


def render_background(theme, cols, rows):
    """Render a theme's background for a cols x rows terminal -> Canvas (cols x 2*rows px)."""
    cv = Canvas(cols, rows * 2)
    base = hex2rgb(theme["palette"]["bg"])
    cv.px = [[base] * cv.w for _ in range(cv.h)]
    for layer in theme.get("background", []):
        LAYERS[layer["type"]](cv, layer)
    return cv


def image_background(path, cols, rows, brightness=1.0, blur=0.0):
    """Image themes: cover-crop + resize the picture to cols x (2*rows) pixels."""
    from PIL import Image, ImageFilter, ImageOps
    im = Image.open(path)
    im.seek(0)                                   # first frame of animated GIF/WebP
    im = ImageOps.exif_transpose(im).convert("RGB")
    if blur:
        im = im.filter(ImageFilter.GaussianBlur(blur * im.width / cols))
    im = ImageOps.fit(im, (cols, rows * 2), Image.Resampling.LANCZOS)
    cv = Canvas(cols, rows * 2)
    data = im.tobytes()
    cv.px = [[tuple(data[(y * cols + x) * 3 + k] / 255 * brightness for k in range(3))
              for x in range(cols)] for y in range(rows * 2)]
    return cv


# ── theme files ───────────────────────────────────────────────────────────────


def load_theme(name_or_path):
    path = name_or_path if os.path.exists(name_or_path) else os.path.join(HERE, f"{name_or_path}.toml")
    with open(path, "rb") as f:
        t = tomllib.load(f)
    t.setdefault("blend", {})
    return t


def builtin_themes():
    return sorted(f[:-5] for f in os.listdir(HERE) if f.endswith(".toml"))


# ── compositing (blocks mode) ─────────────────────────────────────────────────
# Each dashboard cell is (char, fg, bg, bold) plus a *surface* naming what the cell
# belongs to: "screen", "bar", "panel", "select", "chip", "dialog" (or None for
# opaque cells such as solid badges). A surface is drawn as the background seen
# through tinted glass:   out = wall * brightness * (1 - a) + tint * a
# where `a` depends on what is in the cell:
#   blank cell (space)             -> alpha        (lighter: the wallpaper shows)
#   block glyph (▄ ▀ bars)         -> alpha        (the uncovered half is wallpaper)
#   anything else (text, braille)  -> text_alpha   (darker plate behind glyphs), raised
#                                     further by the contrast guard (min_contrast)
# Blank cells become '▀' with fg = top pixel, bg = bottom pixel (2x vertical
# resolution), or a star glyph if the background has a glyph there.
# A foreground colour may also name a surface ("track"): empty bar segments are then
# drawn as glass too, using the pixel under the half that the glyph covers.

UPPER, LOWER = "▀", "▄"


def surface_params(theme, name):
    b = theme["blend"]
    d = b.get(name) or b.get("panel") or {"tint": theme["palette"]["panel"], "alpha": 0.6}
    return hex2rgb(d["tint"]), d.get("alpha", 0.6), d.get("text_alpha", min(1.0, d.get("alpha", 0.6) + 0.2))


def glass(px, tint, a, brightness):
    return tuple(px[k] * brightness * (1 - a) + tint[k] * a for k in range(3))


def guarded(fg, px, tint, a, brightness, min_c):
    """Contrast guard for glyph cells: raise the tint alpha in steps of 0.1 until the
    glyph reaches `min_c` contrast against its background, or the most the pure tint
    allows (90% of contrast(fg, tint)). Bright patches of a wallpaper then get a denser
    plate behind text while dark areas stay see-through."""
    target = min(min_c, contrast(fg, tint) * 0.9)
    bg = glass(px, tint, a, brightness)
    while a < 1.0 and contrast(fg, bg) < target:
        a = min(1.0, a + 0.1)
        bg = glass(px, tint, a, brightness)
    return bg


def composite(cells, theme, cv, stars_on=("screen", "panel", "bar")):
    """cells: rows of dicts {ch, fg, bg, bold, surface, fg_surface}; colours are rgb
    tuples (0..1). Returns rows of (ch, fg_rgb, bg_rgb, bold)."""
    br = theme["blend"].get("brightness", 1.0)
    min_c = theme["blend"].get("min_contrast", 4.5)
    out = []
    params = {}
    for y, row in enumerate(cells):
        line = []
        for x, c in enumerate(row):
            ch, fg, bg, bold = c["ch"], c["fg"], c["bg"], c["bold"]
            surf = c.get("surface")
            top, bot = cv.px[2 * y][x], cv.px[2 * y + 1][x]
            avg = lerp(top, bot, 0.5)
            if surf:
                if surf not in params:
                    params[surf] = surface_params(theme, surf)
                tint, a, ta = params[surf]
                if ch == " ":
                    g = cv.glyphs.get((x, y))
                    # a star only in a blank cell between blank cells, so it can never be
                    # mistaken for a decimal point or a sign next to a number
                    if (g and surf in stars_on and 0 < x < len(row) - 1
                            and row[x - 1]["ch"] == " " and row[x + 1]["ch"] == " "):
                        bgc = glass(avg, tint, a, br)
                        line.append((g[0], lerp(bgc, g[1], 0.85 * (1 - a * 0.6)), bgc, False))
                    else:
                        line.append((UPPER, glass(top, tint, a, br), glass(bot, tint, a, br), False))
                    continue
                if ch == LOWER:
                    bg = glass(top, tint, a, br)
                elif ch == UPPER:
                    bg = glass(bot, tint, a, br)
                else:
                    bg = guarded(fg, avg, tint, ta, br, min_c)
            fs = c.get("fg_surface")
            if fs:
                if fs not in params:
                    params[fs] = surface_params(theme, fs)
                tint, a, ta = params[fs]
                fg = glass(bot if ch == LOWER else top if ch == UPPER else avg, tint, a, br)
            line.append((ch, fg, bg, bold))
        out.append(line)
    return out


# ── palette from an image ─────────────────────────────────────────────────────


def palette_from_image(path, k=8):
    """Derive a theme palette from a picture (see README "Image themes").
    1. downscale to 96x96 (BOX), quantise to k colours with median cut + 3 k-means passes
    2. weight each colour by population; base = darkest heavily populated colour
    3. accents = the two most "vivid" colours (saturation * value * sqrt(share)),
       with distinct hues (> 40° apart where possible)
    4. build a panel tint from the base (darkened to luminance <= 0.02), text colours as
       near-white tinted toward accent 1, and a freq ramp dark-accent -> accent -> light
    Returns a theme dict (palette + blend) that can be merged over a base theme."""
    from PIL import Image
    im = Image.open(path)
    im.seek(0)
    im = im.convert("RGB").resize((96, 96), Image.Resampling.BOX)
    q = im.quantize(colors=k, method=Image.Quantize.MEDIANCUT, kmeans=3)
    pal = q.getpalette()[:3 * k]
    counts = sorted(q.getcolors(), reverse=True)
    total = sum(n for n, _ in counts)
    cols = [((pal[3 * i] / 255, pal[3 * i + 1] / 255, pal[3 * i + 2] / 255), n / total) for n, i in counts]

    def hsv(c):
        return colorsys.rgb_to_hsv(*c)

    base = min((c for c, share in cols if share > 0.05), key=luminance, default=cols[0][0])
    vivid = sorted(cols, key=lambda cs: -(hsv(cs[0])[1] * hsv(cs[0])[2] * math.sqrt(cs[1])))
    acc1 = vivid[0][0]
    acc2 = next((c for c, _ in vivid[1:] if min(abs(hsv(c)[0] - hsv(acc1)[0]),
                                                 1 - abs(hsv(c)[0] - hsv(acc1)[0])) > 40 / 360),
                vivid[1][0] if len(vivid) > 1 else acc1)

    def with_hsv(c, s=None, v=None):
        h, s0, v0 = hsv(c)
        return colorsys.hsv_to_rgb(h, s0 if s is None else s, v0 if v is None else v)

    a1 = with_hsv(acc1, s=min(0.75, max(0.45, hsv(acc1)[1])), v=0.95)
    a2 = with_hsv(acc2, s=min(0.7, max(0.4, hsv(acc2)[1])), v=0.9)
    tint = base
    while luminance(tint) > 0.012:
        tint = tuple(v * 0.85 for v in tint)
    text = lerp((0.93, 0.94, 0.97), a1, 0.12)
    pal_out = {
        "bg": rgb2hex(tint), "panel": rgb2hex(lerp(tint, a1, 0.04)),
        "bar": rgb2hex(lerp(tint, (0, 0, 0), 0.3)), "select": rgb2hex(lerp(tint, a1, 0.22)),
        "chip": rgb2hex(lerp(tint, a1, 0.18)), "track": rgb2hex(lerp(tint, a1, 0.12)),
        "border": rgb2hex(lerp(tint, a1, 0.35)), "border_hi": rgb2hex(lerp(tint, a1, 0.7)),
        "title": rgb2hex(text), "accent": rgb2hex(a1), "accent2": rgb2hex(a2),
        "fg": rgb2hex(text), "text": rgb2hex(lerp(text, tint, 0.18)),
        "mid": rgb2hex(lerp(text, tint, 0.42)), "dim": rgb2hex(lerp(text, tint, 0.62)),
        "freq": [rgb2hex(with_hsv(a2, v=0.45)), rgb2hex(a2), rgb2hex(a1), rgb2hex(lerp(a1, (1, 1, 1), 0.6))],
    }
    blend_out = {
        "brightness": 0.9,
        "screen": {"tint": rgb2hex(tint), "alpha": 0.25, "text_alpha": 0.6},
        "bar": {"tint": rgb2hex(tint), "alpha": 0.6, "text_alpha": 0.75},
        "panel": {"tint": rgb2hex(tint), "alpha": 0.5, "text_alpha": 0.74},
        "select": {"tint": pal_out["select"], "alpha": 0.7, "text_alpha": 0.8},
        "chip": {"tint": pal_out["chip"], "alpha": 0.8, "text_alpha": 0.85},
        "track": {"tint": pal_out["track"], "alpha": 0.72},
        "dialog": {"tint": rgb2hex(tint), "alpha": 0.82, "text_alpha": 0.9},
    }
    return {"palette": pal_out, "blend": blend_out,
            "swatches": [(rgb2hex(c), round(s, 3)) for c, s in cols]}


def image_theme(path, base="default"):
    """A complete theme for an image: the base theme's semantic colours (load/temp
    ramps, status colours) with the image-derived palette and blend merged over it."""
    t = load_theme(base)
    d = palette_from_image(path)
    t["palette"] = {**t["palette"], **d["palette"]}
    t["blend"] = d["blend"]
    t["background"] = []
    t["name"] = "image:" + os.path.basename(path)
    return t


# ── CLI ───────────────────────────────────────────────────────────────────────


def save_png(cv, path, scale=4):
    from PIL import Image
    im = Image.new("RGB", (cv.w, cv.h))
    im.putdata([tuple(round(clamp01(v) * 255) for v in p) for row in cv.px for p in row])
    im.resize((cv.w * scale, cv.h * scale), Image.Resampling.NEAREST).save(path)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("render", help="render a theme background to PNG (half-block pixels)")
    r.add_argument("theme")
    r.add_argument("out")
    r.add_argument("--size", default="172x40", help="terminal size COLSxROWS")
    p = sub.add_parser("palette", help="derive a palette from an image")
    p.add_argument("image")
    sub.add_parser("list", help="list built-in themes")
    c = sub.add_parser("check", help="check text contrast of every theme")
    a = ap.parse_args()
    if a.cmd == "list":
        for n in builtin_themes():
            t = load_theme(n)
            print(f"{n:12} {t['label']:18} {t['description']}")
    elif a.cmd == "render":
        cols, rows = map(int, a.size.split("x"))
        save_png(render_background(load_theme(a.theme), cols, rows), a.out)
    elif a.cmd == "palette":
        import json
        print(json.dumps(palette_from_image(a.image), indent=2))
    elif a.cmd == "check":
        bad = 0
        for n in builtin_themes():
            t = load_theme(n)
            pal = t["palette"]
            panel = hex2rgb(pal["panel"])
            for key in ("fg", "text", "mid", "dim", "accent"):
                cr = contrast(hex2rgb(pal[key]), panel)
                need = 2.5 if key == "dim" else 4.5
                flag = "" if cr >= need else "  <-- low"
                bad += bool(flag)
                print(f"{n:12} {key:7} {cr:5.1f}:1{flag}")
        sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Generate cpuview's original lofi-style wallpapers (created for this project; CC0).

    python3 generate.py OUTDIR

Every scene is deterministic (fixed seeds) and drawn with Pillow only. Pixel-art scenes are
painted at low resolution and upscaled with nearest-neighbour to 1920x1080; the others are
painted at full size. Scenes are kept dark with a few bright accents, which reads well
behind a dashboard (see ../CREDITS.md and docs/image-themes.md).
"""
import math
import os
import random
import sys

from PIL import Image, ImageChops, ImageDraw, ImageFilter

W, H = 1920, 1080


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def hexc(h):
    return tuple(int(h[i:i + 2], 16) for i in (1, 3, 5))


def vgrad(w, h, stops):
    """Vertical gradient image from [(pos, '#rrggbb'), ...]."""
    im = Image.new("RGB", (1, h))
    px = im.load()
    stops = [(p, hexc(c)) for p, c in stops]
    for y in range(h):
        t = y / max(1, h - 1)
        for (p0, c0), (p1, c1) in zip(stops, stops[1:]):
            if p0 <= t <= p1:
                px[0, y] = lerp(c0, c1, (t - p0) / (p1 - p0) if p1 > p0 else 0)
                break
        else:
            px[0, y] = stops[-1][1] if t > stops[-1][0] else stops[0][1]
    return im.resize((w, h))


def glow(im, cx, cy, r, color, strength=1.0):
    """Additive soft radial glow."""
    layer = Image.new("RGB", im.size, (0, 0, 0))
    d = ImageDraw.Draw(layer)
    c = hexc(color)
    for k in range(24, 0, -1):
        rr = r * k / 24
        a = strength * (1 - k / 24) ** 2
        d.ellipse([cx - rr, cy - rr, cx + rr, cy + rr], fill=tuple(round(v * a) for v in c))
    layer = layer.filter(ImageFilter.GaussianBlur(r / 10))
    return _add(im, layer)


def _add(a, b):
    return ImageChops.add(a, b)


def up(im):
    return im.resize((W, H), Image.Resampling.NEAREST)


# ── 1. pixel night city ───────────────────────────────────────────────────────

def night_city():
    rnd = random.Random(11)
    w, h = 480, 270
    im = vgrad(w, h, [(0, "#070a1c"), (0.55, "#1a1640"), (0.8, "#3a1f4f"), (1, "#120a1c")])
    d = ImageDraw.Draw(im)
    for _ in range(140):                                  # stars, fewer near the horizon
        x, y = rnd.randrange(w), int(rnd.random() ** 1.6 * h * 0.6)
        b = rnd.choice([90, 120, 160, 220])
        d.point((x, y), fill=(b, b, min(255, b + 30)))
    im = glow(im, 370, 52, 70, "#6a5a9a", 0.8)            # moon halo
    d = ImageDraw.Draw(im)
    d.ellipse([356, 38, 384, 66], fill=(236, 232, 214))
    d.ellipse([364, 40, 390, 64], fill=(214, 208, 190))   # soft terminator
    d.ellipse([356, 38, 384, 66], outline=(250, 248, 236))
    layers = [  # (skyline base, height range, width range, body colour, window palette, lit chance)
        (205, (40, 110), (14, 30), (28, 26, 58), [(120, 110, 190), (90, 140, 200)], 0.12),
        (225, (40, 120), (16, 34), (18, 16, 36), [(255, 196, 120), (255, 150, 200), (140, 220, 255)], 0.2),
        (262, (30, 150), (20, 46), (9, 8, 18), [(255, 210, 130), (255, 170, 90), (120, 230, 255), (255, 120, 190)], 0.28),
    ]
    for base, (h0, h1), (w0, w1), body, wins, lit in layers:
        x = -rnd.randrange(10)
        while x < w:
            bw, bh = rnd.randrange(w0, w1), rnd.randrange(h0, h1)
            top = base - bh
            d.rectangle([x, top, x + bw, h], fill=body)
            if rnd.random() < 0.3:                        # roof details
                d.rectangle([x + bw // 3, top - 6, x + bw // 3 + 2, top], fill=body)
                if rnd.random() < 0.5:
                    d.point((x + bw // 3 + 1, top - 7), fill=(255, 80, 80))
            for wy in range(top + 4, h - 4, 5):
                for wx in range(x + 3, x + bw - 3, 4):
                    if rnd.random() < lit:
                        c = rnd.choice(wins)
                        d.rectangle([wx, wy, wx + 1, wy + 2], fill=c)
            if rnd.random() < 0.12 and base > 240:        # neon sign
                c = rnd.choice([(255, 80, 180), (80, 230, 255), (255, 200, 80)])
                sx, sy = x + 3, top + 10
                d.rectangle([sx, sy, sx + 2, sy + 14], fill=c)
            x += bw + rnd.randrange(0, 4)
    im = glow(im, 240, 300, 260, "#5a2a6a", 0.6)          # city light pollution
    return up(im)


# ── 2. synthwave sunset ───────────────────────────────────────────────────────

def synthwave():
    w, h = 960, 540
    hz = int(h * 0.62)
    im = vgrad(w, h, [(0, "#0a0322"), (0.35, "#2a0b4a"), (0.55, "#8a1f6b"), (0.62, "#ff7a45"),
                      (0.6201, "#14051f"), (1, "#05020a")])
    rnd = random.Random(5)
    d = ImageDraw.Draw(im)
    for _ in range(220):
        x, y = rnd.randrange(w), int(rnd.random() ** 1.5 * hz * 0.85)
        b = rnd.choice([100, 150, 210])
        d.point((x, y), fill=(b, b - 20, b))
    im = glow(im, w // 2, int(hz * 0.78), 300, "#ff3f8f", 0.7)
    d = ImageDraw.Draw(im)
    cx, cy, r = w // 2, int(hz * 0.78), 150
    sun = vgrad(2 * r, 2 * r, [(0, "#ffe66d"), (0.45, "#ff9f43"), (0.75, "#ff4f8b"), (1, "#c4217a")])
    mask = Image.new("L", (2 * r, 2 * r), 0)
    md = ImageDraw.Draw(mask)
    md.ellipse([0, 0, 2 * r - 1, 2 * r - 1], fill=255)
    for k in range(7):                                    # stripes in the lower half
        y0 = r + k * r // 7 + 4
        md.rectangle([0, y0, 2 * r, y0 + 2 + k], fill=0)
    im.paste(sun, (cx - r, cy - r), mask)
    d = ImageDraw.Draw(im)
    d.rectangle([0, hz, w, h], fill=(16, 5, 28))
    # mountains on the horizon
    for col, amp, seed in (((44, 12, 70), 70, 3), ((24, 6, 40), 45, 9)):
        rr = random.Random(seed)
        pts, x = [(0, hz)], 0
        while x <= w:
            pts.append((x, hz - rr.randrange(5, amp)))
            x += rr.randrange(30, 90)
        pts += [(w, hz), (0, hz)]
        d.polygon(pts, fill=col)
    grid = Image.new("RGB", (w, h), (0, 0, 0))
    gd = ImageDraw.Draw(grid)
    for i in range(1, 22):                                # horizontal lines, perspective spacing
        y = hz + (h - hz) * (i / 21) ** 2.2
        gd.line([(0, y), (w, y)], fill=(255, 60, 200), width=2)
    for i in range(-24, 25):                              # converging verticals
        gd.line([(cx + i * 12, hz), (cx + i * 110, h)], fill=(255, 60, 200), width=2)
    gmask = vgrad(w, h, [(0, "#000000"), (0.62, "#000000"), (0.66, "#303030"), (1, "#ffffff")]).convert("L")
    grid = Image.composite(grid, Image.new("RGB", (w, h)), gmask)
    im = _add(im, grid.filter(ImageFilter.GaussianBlur(1.2)))
    im = _add(im, grid.filter(ImageFilter.GaussianBlur(6)))
    return im.resize((W, H), Image.Resampling.LANCZOS)


# ── 3. rainy window at night ──────────────────────────────────────────────────

def rainy_window():
    rnd = random.Random(23)
    im = vgrad(W, H, [(0, "#0a1020"), (0.6, "#121a30"), (1, "#0a0d18")])
    bokeh = Image.new("RGB", (W, H))
    d = ImageDraw.Draw(bokeh)
    cols = ["#ffb35c", "#ff7a7a", "#7ad0ff", "#ffd27a", "#c08aff", "#5cffd0"]
    for _ in range(90):                                   # out-of-focus city lights
        x, y = rnd.randrange(W), int(H * (0.35 + 0.55 * rnd.random()))
        r = rnd.randrange(14, 70)
        c = hexc(rnd.choice(cols))
        k = rnd.uniform(0.25, 0.7)
        d.ellipse([x - r, y - r, x + r, y + r], fill=tuple(round(v * k) for v in c))
    im = _add(im, bokeh.filter(ImageFilter.GaussianBlur(18)))
    drops = Image.new("RGB", (W, H))
    d = ImageDraw.Draw(drops)
    for _ in range(1400):                                 # rain streaks
        x, y = rnd.randrange(W), rnd.randrange(H)
        ln = rnd.randrange(20, 70)
        b = rnd.randrange(30, 80)
        d.line([(x, y), (x - ln * 0.12, y + ln)], fill=(b, b + 10, b + 30), width=1)
    for _ in range(260):                                  # droplets on the glass
        x, y = rnd.randrange(W), rnd.randrange(H)
        r = rnd.randrange(2, 7)
        d.ellipse([x - r, y - r, x + r, y + r], outline=(90, 110, 150))
        d.point((x - r // 2, y - r // 2), fill=(200, 220, 255))
    im = _add(im, drops.filter(ImageFilter.GaussianBlur(0.8)))
    frame = ImageDraw.Draw(im)                            # window frame, dark
    frame.rectangle([0, 0, W, 38], fill=(6, 8, 14))
    frame.rectangle([0, H - 60, W, H], fill=(6, 8, 14))
    frame.rectangle([W // 2 - 14, 0, W // 2 + 14, H], fill=(6, 8, 14))
    return im


# ── 4. dusk mountains ─────────────────────────────────────────────────────────

def dusk_mountains():
    w, h = 480, 270
    im = vgrad(w, h, [(0, "#14112e"), (0.35, "#3b2350"), (0.55, "#a5486a"), (0.68, "#f08a5d"), (1, "#f8b26a")])
    im = glow(im, 330, 182, 90, "#ffcf8a", 0.9)
    d = ImageDraw.Draw(im)
    d.ellipse([318, 170, 342, 194], fill=(255, 226, 170))
    rnd = random.Random(8)
    for i, (col, base, amp) in enumerate(((("#7a3f63"), 175, 40), ("#552e55", 195, 45),
                                          ("#382344", 215, 45), ("#231833", 235, 40), ("#120c1e", 255, 35))):
        pts, x = [(0, h)], -10
        y = base - rnd.randrange(amp)
        while x <= w + 10:
            pts.append((x, y))
            x += rnd.randrange(6, 22)
            y = max(base - amp, min(base + 6, y + rnd.randrange(-14, 15)))
        pts += [(w + 10, h)]
        d.polygon(pts, fill=hexc(col))
        if i < 3:  # haze between ridges
            haze = Image.new("RGB", (w, h), (0, 0, 0))
            hd = ImageDraw.Draw(haze)
            hd.rectangle([0, base - 5, w, base + 18], fill=(60, 30, 50))
            im = _add(im, haze.filter(ImageFilter.GaussianBlur(6)))
            d = ImageDraw.Draw(im)
    for _ in range(40):
        d.point((rnd.randrange(w), rnd.randrange(60)), fill=(200, 190, 220))
    for k in range(5):  # birds
        bx, by = 120 + k * 9 + rnd.randrange(5), 90 + rnd.randrange(12)
        d.line([(bx - 3, by - 1), (bx, by), (bx + 3, by - 1)], fill=(40, 20, 40))
    return up(im)


# ── 5. cozy desk at night ─────────────────────────────────────────────────────

def cozy_desk():
    w, h = 320, 180
    im = Image.new("RGB", (w, h), (14, 12, 22))
    d = ImageDraw.Draw(im)
    d.rectangle([0, 0, w, 120], fill=(22, 18, 34))                     # wall
    # window with night sky and moon
    wx0, wy0, wx1, wy1 = 168, 14, 300, 96
    sky = vgrad(wx1 - wx0, wy1 - wy0, [(0, "#0b1030"), (1, "#2a2050")])
    im.paste(sky, (wx0, wy0))
    rnd = random.Random(3)
    for _ in range(30):
        d.point((rnd.randrange(wx0, wx1), rnd.randrange(wy0, wy1 - 30)), fill=(200, 200, 240))
    d.ellipse([268, 22, 284, 38], fill=(240, 232, 200))
    for bx in range(wx0, wx1, 9):                                       # distant skyline
        bh = rnd.randrange(8, 26)
        d.rectangle([bx, wy1 - bh, bx + 7, wy1], fill=(16, 14, 34))
        for yy in range(wy1 - bh + 2, wy1 - 1, 3):
            if rnd.random() < 0.3:
                d.point((bx + rnd.randrange(1, 6), yy), fill=(255, 200, 120))
    d.rectangle([wx0 - 3, wy0 - 3, wx1 + 3, wy1 + 3], outline=(40, 30, 48), width=3)
    d.line([((wx0 + wx1) // 2, wy0), ((wx0 + wx1) // 2, wy1)], fill=(40, 30, 48), width=2)
    d.rectangle([wx0 - 6, wy1 + 3, wx1 + 6, wy1 + 6], fill=(48, 36, 52))  # sill
    # desk
    d.rectangle([0, 128, w, 136], fill=(70, 44, 36))
    d.rectangle([0, 136, w, h], fill=(36, 24, 24))
    # monitor
    d.rectangle([40, 70, 120, 120], fill=(10, 10, 16))
    d.rectangle([44, 74, 116, 116], fill=(18, 40, 60))
    for i, yy in enumerate(range(80, 112, 5)):
        d.line([(48, yy), (48 + 20 + (i * 17) % 44, yy)], fill=(80, 200, 220) if i % 3 else (255, 140, 200))
    d.rectangle([76, 120, 84, 128], fill=(20, 18, 26))
    d.rectangle([64, 126, 96, 129], fill=(20, 18, 26))
    # lamp
    d.line([(150, 128), (150, 92)], fill=(30, 26, 34), width=2)
    d.line([(150, 92), (136, 80)], fill=(30, 26, 34), width=2)
    d.polygon([(124, 78), (146, 76), (140, 86), (128, 88)], fill=(200, 120, 70))
    d.rectangle([142, 126, 158, 129], fill=(30, 26, 34))
    # mug + steam, plant, books
    d.rectangle([228, 116, 238, 128], fill=(220, 150, 140))
    d.arc([236, 118, 244, 126], 270, 90, fill=(220, 150, 140))
    for k in range(3):
        d.point((231 + k * 2, 110 - k * 3), fill=(120, 110, 130))
    d.rectangle([258, 116, 272, 128], fill=(150, 80, 60))
    for k in range(9):
        a = -1.2 + k * 0.3
        d.line([(265, 116), (265 + math.sin(a) * 14, 116 - math.cos(a) * 16)], fill=(70, 140, 80), width=2)
    for i, c in enumerate([(120, 60, 80), (60, 90, 130), (150, 120, 60), (80, 60, 110)]):
        d.rectangle([180 + i * 7, 108 - i % 2 * 4, 185 + i * 7, 128], fill=c)
    im = glow(im, 134, 100, 70, "#ff9a4a", 0.85)                        # lamp light
    im = glow(im, 80, 95, 50, "#2a8aa8", 0.5)                           # monitor light
    return up(im)


SCENES = {
    "lofi-night-city": (night_city, "png"),
    "lofi-synthwave": (synthwave, "jpg"),
    "lofi-rainy-window": (rainy_window, "jpg"),
    "lofi-dusk-mountains": (dusk_mountains, "png"),
    "lofi-cozy-desk": (cozy_desk, "png"),
}


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    os.makedirs(out, exist_ok=True)
    only = sys.argv[2:]
    for name, (fn, ext) in SCENES.items():
        if only and name not in only:
            continue
        im = fn()
        path = os.path.join(out, f"{name}.{ext}")
        if ext == "png":
            im.quantize(colors=256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).save(path, optimize=True)
        else:
            im.save(path, quality=86, optimize=True, progressive=True)
        print(path, os.path.getsize(path) // 1024, "KB")


if __name__ == "__main__":
    main()

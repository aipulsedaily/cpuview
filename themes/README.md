# cpuview themes

Each `*.toml` file here is one built-in theme: a palette, how panels blend over the
background ("glass"), and a stack of procedural background layers. The files are pure data;
`reference.py` is the executable specification. When this document and the code disagree,
the code wins. Rendering the same theme with the same terminal size must give the same
picture in any implementation, because all randomness comes from the integer hash below.

```sh
python3 themes/reference.py list                         # built-in themes
python3 themes/reference.py render nebula out.png --size 172x40
python3 themes/reference.py palette wallpapers/nasa-iss-aurora.jpg   # image palette (needs Pillow)
python3 themes/reference.py check                         # text contrast of every palette
```

| name | label | look |
|---|---|---|
| `default` | Midnight | clean ink-blue gradient, soft corner light, faint clouds |
| `space` | Space | black-blue void, Milky Way band with dust lanes, glyph starfield |
| `nebula` | Nebula | violet / magenta / teal domain-warped clouds, dust, stars |
| `aurora` | Aurora | green→violet curtains over two mountain ridges |
| `synthwave` | Synthwave Sunset | striped sun, horizon haze, neon perspective grid |
| `ocean` | Deep Ocean | light rays, surface caustics, drifting particles |
| `forest` | Night Forest | three pine ridges, fog band, moonlight, fireflies |
| `ember` | Ember | dark red smoke, heat glow, rising sparks |
| `mono` | Mono | graphite with a dot grid; red only for critical values |
| `transparent` | Terminal background | no background at all: the terminal default shows through |

## File format

```toml
name = "space"          # id: --theme space, config `theme = "space"`
label = "Space"         # shown in the theme menu
description = "..."     # one line, shown in the menu
schema = 1
transparent = false     # true only for `transparent`: see "Surfaces" below

[palette]               # "#rrggbb" everywhere
bg, panel, bar, select, chip, track     # solid surface colours (used as-is when no background is drawn)
border, border_hi                       # panel borders; border_hi = focused panel / dialog / selection marker
title, fg, text, mid, dim               # text hierarchy, brightest → faintest
accent, accent2                         # brand chip, selection marker, highlights
good, warn, crit                        # status colours (live dot, warnings, PAUSED, hot values)
freq = [c0, c1, c2, c3]                 # clock-speed gradient, low → high (evenly spaced stops)
load = [c0, c1, c2, c3]                 # load gradient, idle → saturated
temp = [c0, c1, c2, c3]                 # temperature gradient, cool → hot (30 °C → 95 °C)

[blend]
brightness = 1.0        # wallpaper multiplier applied before tinting (dims busy images)
min_contrast = 4.5      # optional, contrast guard target for text cells (default 4.5)
screen = { tint = "#..", alpha = 0.0, text_alpha = 0.3 }   # one entry per surface, see below
bar    = { ... }  panel = { ... }  select = { ... }  chip = { ... }  track = { alpha = .. }  dialog = { ... }

[[background]]          # layers, painted in order onto a canvas filled with palette.bg
type = "gradient"
...
```

Gradients (`freq`, `load`, `temp`, and layer `stops`/`colors`) are either a list of colours
(evenly spaced) or a list of `[position, colour]` pairs with positions in 0..1. Values between
stops are interpolated linearly in sRGB (`ramp()` in the reference).

Load and temperature gradients keep their meaning across themes (calm → hot). Themes change
the hue, never the direction. Bar fill colours are sampled per cell, so a bar shows the gradient
up to its current value.

## Surfaces and the glass model

Every cell the dashboard draws belongs to a **surface**: `screen` (bare background), `bar`
(title and footer rows), `panel` (inside and on the border of a panel), `select` (the selected
core's rows), `chip` (key hints in the footer), `dialog` (the theme menu), or none (opaque
things such as the brand badge, which use their palette colour directly). Empty bar segments
use the `track` surface as their *foreground* colour.

In blocks mode the background is drawn at **2 pixels per cell** (the upper and lower half of
the cell) and seen through tinted glass:

```
glass(pixel, surface, a) = pixel * blend.brightness * (1 - a) + surface.tint * a
```

Which `a` applies depends on what the cell contains:

| cell content | output |
|---|---|
| blank (space) | `▀` with fg = glass(top pixel, alpha), bg = glass(bottom pixel, alpha), so the wallpaper shows at double vertical resolution. If the background has a glyph star there *and* both horizontal neighbours are blank, draw the star glyph instead (fg = star colour mixed over the glass, bg = glass(average)) |
| `▄` (lower half glyph, bars) | bg = glass(top pixel, alpha), because the visible background is the upper half |
| `▀` (upper half glyph) | bg = glass(bottom pixel, alpha) |
| anything else (letters, digits, braille, box drawing) | bg = glass(average of both pixels, text_alpha), then the **contrast guard**: while `contrast(fg, bg) < min(min_contrast, 0.9 · contrast(fg, tint))` and a < 1, raise a by 0.1 |
| fg is the `track` surface | fg = glass(pixel under the half the glyph covers, track.alpha) |

`alpha` (blank cells) is kept low so the wallpaper reads clearly in empty space; `text_alpha`
is higher so glyphs sit on a darker plate; the contrast guard only works harder where the
wallpaper is bright. Contrast is WCAG relative-luminance contrast (`contrast()` in the
reference). `transparent = true` themes skip all of this: every surface cell is emitted with
the terminal's **default** background (no SGR 48), so the terminal's own background image or
transparency shows through.

When no background can be drawn (16-colour terminals, or image mode `off`) the solid palette
colours are used as-is: `bg`, `panel`, `bar`, `select`, `chip`, `track`.

## Canvas and coordinates

For a terminal of `cols × rows` cells the canvas is `W = cols`, `H = 2·rows` pixels (pixel
`(x, 2r)` is the upper half of cell `(x, r)`, `(x, 2r+1)` the lower). Half-cell pixels are
treated as square. Layers see, for pixel `(x, y)`:

```
u = (x + 0.5) / W        # 0..1 across
v = (y + 0.5) / H        # 0..1 down
s = (x + 0.5) / H        # across, in units of the height (keeps circles and noise isotropic)
```

Colour channels are floats in 0..1. Layer blend modes: `over` (`d + (s − d)·α`), `add`
(`min(1, d + s·α)`), `screen` (`d + (1 − d)·s·α`), `multiply` (`d·(1 − α + s·α)`).

Canvases are rendered once per (theme, terminal size) and cached. A 172×40 terminal is a
172×80 canvas: about 14 k pixels.

### Hash and noise (bit-exact)

```
hash_u32(ix, iy, seed):          # all arithmetic mod 2^32 (Rust: wrapping_mul / wrapping_add, i64→u32 for negatives)
    h = ix*374761393 + iy*668265263 + seed*2246822519
    h = (h ^ (h >> 13)) * 1274126177
    return h ^ (h >> 16)
hash01 = hash_u32 / 0xFFFFFFFF                     # [0, 1]

fade(t) = t³(t(6t − 15) + 10)                     # quintic smoothstep
value_noise(x, y, seed):                           # lattice values hash01(ix, iy, seed), bilinear with fade()
fbm(x, y, seed, octaves=5, lacunarity=2, gain=0.5, ridged=false):
    sum over k < octaves of amp_k · n_k,  n_k = value_noise(x·f_k, y·f_k, seed + k)
    (ridged: n_k = 1 − |2 n_k − 1|),  f_k = lacunarity^k,  amp_k = gain^k,  divided by Σ amp_k
contrast_curve(n, threshold, power) = clamp((n − threshold)/(1 − threshold), 0, 1) ^ power
```

Python ints are unbounded, so the reference masks with `& 0xFFFFFFFF` after each step.
Negative lattice coordinates (domain warping can produce them) go through Python's floor and
multiply, then the mask. A port must wrap the same way: convert the product to u32 the way
two's complement does.

### Layer types

All parameters are optional unless marked *required*. Defaults are in the reference.

| type | parameters | algorithm |
|---|---|---|
| `gradient` | `stops`*, `angle` (deg, 0 = top→bottom, 90 = left→right), `blend`=over, `opacity`=1 | t = projection of (u, v) on the direction, normalised over the four corners |
| `radial` | `stops`* (centre→rim), `center` [u, v], `radius` (height units), `falloff`=1.5, `blend`=screen, `opacity` | d = dist((s, v), (center.u·W/H, center.v)) / radius; if d < 1 blend ramp(d) with α = opacity·(1 − d)^falloff |
| `noise` | `seed`, `scale`=3, `octaves`, `lacunarity`, `gain`, `ridged`, `warp`=0, `stretch`=[1,1], `colors`*, `threshold`, `power`, `band`, `blend`=screen, `opacity` | (nx, ny) = (s·scale·stretch.x, v·scale·stretch.y); if warp: nx += warp·2(fbm(nx+5.2, ny+1.3, seed+101, 3) − .5), ny += warp·2(fbm(nx+1.7, ny+9.2, seed+202, 3) − .5) (both lookups use the *unwarped* coordinates); m = contrast_curve(fbm(nx, ny, seed, …)) · band(u, v); blend ramp(colors)(m) with α = opacity·m |
| *band* (inside noise) | `{center, slope, width}` | band(u, v) = exp(−((v − (center + slope·(u − 0.5)))/width)²) |
| `stars` | `seed`, `density` (per pixel), `density_gradient`, `min_brightness`, `colors`, `glyph_density` (per cell), `glyphs` | dens(v) = max(0, 1 + gradient·(v − 0.5)·2). **Pixel stars only on hi-res canvases** (sixel/kitty/iterm renders), where one canvas pixel is one screen pixel: pixel is a star if hash01(x, y, seed) < density·dens(v); brightness b = min_b + (1 − min_b)·hash01(x, y, seed+1)³; colour colors[⌊hash01(x, y, seed+2)·n⌋]; blend add with α = b. **Glyph stars** (all modes): cell (cx, cy) gets one if hash01(cx, cy, seed+3) < glyph_density·dens(v_cell); glyph = glyphs[⌊hash01(cx, cy, seed+4)²·n⌋]; b = min_b + (1 − min_b)·hash01(cx, cy, seed+5)²; colour = colors[⌊hash01(cx, cy, seed+6)·n⌋]·b. Stored per cell, drawn by the compositor (see glass model) |
| `aurora` | `seed`, `opacity`, `bands` = [{`y`, `amplitude`, `waves`, `phase`, `height`, `streaks`, `bottom_color`, `top_color`, `intensity`}] | per band i (seed_i = seed + 31i): yc(u) = y + amp·sin(2π(u·waves + phase)) + amp·(fbm(u·3, 0, seed_i, 3) − .5); rays(u) = 0.55 + 0.45·value_noise(u·streaks, 0.5, seed_i + 9); d = yc − v; k = rays·exp(−d/height) if d > 0 else exp(d/(0.15·height)); add ramp([bottom, top])(clamp(d/height)) with α = opacity·k·intensity (skip k ≤ 0.004) |
| `sun` | `center`, `radius`, `stops`* (top→bottom of disc), `stripes`, `glow`, `glow_size`=1.2, `glow_opacity`=0.35, `clip_below` | skip v > clip_below. d = dist to centre (s units); t = (v − (cy − r))/(2r). Inside the disc: in the lower half (t > .5), k = 2(t − .5), stripe gap when ((k mod 1/stripes)·stripes) < 0.15 + 0.55k; else paint ramp(t). Outside or in a gap: add glow with α = glow_opacity·g², g = 1 − (d − r)/(r·glow_size) outside (≥ 0), 0.35 in gaps |
| `grid` | `horizon`, `color`, `density`, `columns`, `line`, `depth_scale`, `fade_distance`, `opacity` | below the horizon, dz = v − horizon: z = depth_scale/dz; fz = frac(z·density); fx = frac((u − .5)/dz·columns·0.1); h = max(0, 1 − min(fz, 1 − fz)/line); vline = max(0, 1 − min(fx, 1 − fx)/(1.6·line)); add colour with α = opacity·max(h, vline)·clamp(dz/fade_distance) |
| `silhouette` | `kind` mountains/trees, `seed`, `base`, `height`, `roughness`, `color`*, `rim`, `opacity`; trees: `tree_width` (cols), `tree_height`, `tree_fill`=0.85 | ridge(x): mountains = base − height·fbm(u·roughness, 0, seed, 5). Trees = base − 0.3·height·fbm(u·roughness, 0, seed, 3), then for buckets b ∈ {⌊x/tw⌋−1, ⌊x/tw⌋, ⌊x/tw⌋+1} with hash01(b, 0, seed+11) < tree_fill: centre = (b + hash01(b,1,·))·tw, h = tree_height·(0.5 + hash01(b,2,·)), half = 0.75·tw·(0.7 + 0.6·hash01(b,3,·)), dx = \|x + .5 − centre\|/half; if dx < 1: ridge = min(ridge, base − h(1 − dx) + 0.08h·hash01(x, 7, seed+13)). Pixels with v ≥ ridge get `color` (over, opacity); the topmost pixel adds `rim` at 0.6 |
| `rays` | `center` [u, v] (usually v < 0), `color`, `count`, `sharpness`, `reach`, `seed`, `opacity` | ang = atan2(s − cx, v − cy); k = value_noise(ang·count, 0, seed)^sharpness · exp(−dist/reach); add with α = opacity·k |
| `vignette` | `strength` | multiply by max(0, 1 − strength·r²/2), r² = (2u − 1)² + (2v − 1)² |
| `dotgrid` | `every` [cols, rows], `glyph`, `color` | glyph in every cell with cx mod cols = ⌊cols/2⌋ and cy mod rows = 0 (glyph layer, like stars) |

Glyphs used by the built-ins (`· ∙ • °`) are all in Cascadia Mono, the Windows Terminal
default font. Avoid `✦ ★ ☆ ✶ ✳ ❄ ☾ ⋅ ✕`: Cascadia lacks them, so they fall back to another
font and can misalign.

## Image themes

A picture becomes a theme with the same machinery:

1. **Background**: load the image (first frame of GIF/WebP, EXIF orientation applied, alpha
   flattened), optional Gaussian blur, then cover-crop and resize (Lanczos) to `W × H` canvas
   pixels. Cache the result per (image, size, blur). For hi-res modes resize to the terminal's
   pixel size instead (see `docs/image-themes.md`).
2. **Palette** (`palette_from_image`): downscale to 96×96 (box filter), quantise to 8 colours
   (median cut + 3 k-means passes), and weight each colour by its pixel share.
   - *base* = darkest colour with ≥ 5 % share; the tint is the base darkened by ×0.85 steps
     until luminance ≤ 0.012.
   - *accent1* = most vivid colour by saturation · value · √share; *accent2* = next vivid with
     hue > 40° away (else the second most vivid). Saturation is clamped to 0.45–0.75 and value
     set to 0.95/0.90.
   - text = near-white mixed 12 % toward accent1; text/mid/dim = text mixed 18/42/62 % toward
     the tint; borders = tint→accent1 at 35 % (70 % for `border_hi`); freq ramp = accent2 at
     value 0.45 → accent2 → accent1 → accent1 lightened 60 %.
   - Load/temp ramps and status colours come from the base theme (`default`), so meanings
     don't shift with the picture.
   - Blend defaults for images: brightness 0.9, panel alpha 0.5 / text 0.74, screen 0.25,
     bar 0.6, select 0.7, track 0.72, dialog 0.82.
3. **Per-image options** (stored per file in the config): `dim` (overrides
   `blend.brightness`, default 0.9), `blur` (radius in canvas pixels, default 0).

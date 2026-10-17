# Image themes: design notes

Design reference for wallpaper and image backgrounds in cpuview: how to detect what a
terminal can do (over SSH), which mode to pick, and how each mode coexists with a TUI that
redraws text many times a second. The code references are:

- `themes/reference.py`: backgrounds, the glass compositor (blocks mode), palette from an image
- `docs/reference/termgfx.py`: terminal probe, mode selection, sixel / kitty / iTerm2 encoders,
  tmux wrapping, and a `doctor` command (`python3 docs/reference/termgfx.py doctor`)

Status is at the end: what was verified headlessly and what still needs a real terminal.

## Modes

| mode | what the user sees | where it works |
|---|---|---|
| `kitty` | full-resolution picture **under** the text (kitty z-index layering), panels tinted with translucent overlays, a plate under text only where the picture is too bright to read on | kitty, Ghostty (WezTerm implements the protocol but erases images under text, wezterm#7222: auto picks sixel there) |
| `sixel` | the whole picture at the window's pixel resolution in every cell without text (blank cells, panel interiors, graph grid lines, panel frames and bar tracks drawn into it); text cells take the picture's colour | Windows Terminal ≥ 1.22, foot, WezTerm, Konsole, mlterm, contour, xterm `-ti vt340`, mintty, VS Code (sixel enabled), tmux ≥ 3.4 built with sixel |
| `iterm` | like sixel, with PNG patches via OSC 1337 | iTerm2, WezTerm, mintty, VS Code |
| `blocks` | the picture drawn with block glyphs, the best two-colour split of each cell: octants (2×4 pixels), sextants (2×3), quadrants (2×2) or `▀` half blocks (1×2), truecolor | any truecolor terminal (the guaranteed path) |
| `blocks256` | the same, ordered-dithered to the xterm-256 palette | 256-colour terminals |
| `terminal` | cpuview draws **no** background: surface cells use the terminal's default background, so the terminal's own background image shows through | Windows Terminal `backgroundImage`, kitty `background_image`, WezTerm `background`, iTerm2 profile image, any translucent terminal |
| `off` | solid palette colours | everything, including 16-colour terminals |

`--image-mode auto|kitty|sixel|iterm|blocks|blocks256|terminal|off`, config key `image_mode`,
and a row in the theme menu. `auto` never picks `terminal`.

## Detecting capabilities over SSH

Environment variables such as `TERM_PROGRAM`, `WT_SESSION`, `KITTY_WINDOW_ID` and often
`COLORTERM` are **not** forwarded by SSH. Treat them as hints only. The reliable signal is to
ask the terminal: escape sequences travel through SSH like any output.

**Procedure** (`probe()`), before the TUI takes over the tty:

1. Open `/dev/tty`, switch to cbreak with echo off, and enter the alternate screen
   (`CSI ?1049h`), so a terminal that prints an unknown sequence as text only does it on a
   screen that is about to be replaced.
2. Write **all** queries in one `write()`, DA1 last:

   | query | bytes | reply |
   |---|---|---|
   | XTVERSION | `ESC [ > 0 q` | `ESC P > \| name(version) ESC \` e.g. `kitty(0.36.4)`, `WezTerm 20240203-…`, `foot(1.18.1)`, `XTerm(390)`, `tmux 3.4`, `iTerm2 3.5.0`, `Konsole 24.02.1` |
   | kitty graphics | `ESC _ G i=31,s=1,v=1,a=q,t=d,f=24 ; AAAA ESC \` | `ESC _ G i=31;OK ESC \` (or an error text) |
   | cell size | `ESC [ 16 t` | `ESC [ 6 ; height ; width t` (pixels) |
   | text area size | `ESC [ 14 t` | `ESC [ 4 ; height ; width t` |
   | truecolor | `ESC [ 48;2;1;2;3 m` then DECRQSS `ESC P $ q m ESC \` then `ESC [ 0 m` | `ESC P 1 $ r … 48:2::1:2:3 m ESC \` or `…48;2;1;2;3m`: the colour survived, so it is truecolor |
   | DA1 (sentinel) | `ESC [ c` | `ESC [ ? 6x ; … c`; attribute **4** = sixel |

3. Read with `select()` until the DA1 reply arrives. Terminals answer in order, so once DA1 is
   in, everything else that will come has arrived. Timeout **300 ms**. If some replies have
   started but DA1 hasn't (a slow link), extend to **1 s**. Measured through a pty with a fake
   Windows Terminal: about 50 ms. A silent terminal costs 320 ms.
4. Leave the alternate screen, `tcflush(TCIFLUSH)` to drop stragglers, and restore the
   termios settings.

Residual risk: a reply that arrives *after* the flush stays in the tty line buffer and is read
by the TUI once it switches to raw mode. The app's input parser must ignore unknown
CSI/DCS/APC sequences (Textual does). Offer `--no-probe` and `CPUVIEW_PROBE_TIMEOUT`.

**Inside tmux**, XTVERSION is answered by tmux itself (`tmux 3.4`). The kitty query only
reaches the outer terminal when wrapped in passthrough (below), so `probe()` wraps it when
`$TMUX` is set.

**Cell size** in pixels, in order: `CSI 16 t`; `CSI 14 t` divided by the grid; `TIOCGWINSZ`
`ws_xpixel/ws_ypixel` divided by the grid (often 0 over SSH); for sixel, fall back to **10×20**.
That is the VT340 cell size, and Windows Terminal uses it as its virtual cell for scaling sixel
images.

### Selection order (`choose_mode`)

```
explicit --image-mode / config              → use it (sixel/kitty/iterm inside GNU screen → blocks)
no replies at all                           → blocks if COLORTERM=truecolor|24bit, blocks256 if TERM *256color*, else off
kitty OK and name ∈ {kitty, ghostty} and not tmux   → kitty
DA1 has 4                                   → sixel   (inside tmux: tmux ≥ 3.4 renders sixel itself)
kitty OK (other terminals), not tmux        → kitty
name ∈ {iTerm2, WezTerm, mintty, vscode}    → iterm
truecolor (COLORTERM, DECRQSS echo, or a known modern name) → blocks
TERM *256color*                             → blocks256
otherwise                                   → off
```

Kitty is preferred only where it is known to layer images under text. Other terminals
answer the kitty query too (Konsole, for example) without that guarantee, and they also
support sixel. The selection matrix is unit-tested in `termgfx.py selftest` with recorded or
documented reply shapes: kitty, WezTerm, Windows Terminal 1.21/1.22, conhost, foot, Konsole,
iTerm2, xterm (default and vt340), tmux with and without sixel, and a silent terminal.

### `cpuview --doctor`

Prints everything above: terminal name and version, TERM/COLORTERM, whether it answered
and how fast, DA1 attributes, truecolor/sixel/kitty, cell size and how it was found, tmux,
the chosen mode and **why**. It then draws a test strip in that mode so the user can confirm
by eye. `python3 docs/reference/termgfx.py doctor` / `demo sixel` is the working prototype.

### Truecolor over SSH (a finding that affects the current app too)

Textual's color system defaults to `auto`, which is Rich's environment check: truecolor only
if `COLORTERM` is `truecolor`/`24bit`. Windows OpenSSH doesn't send `COLORTERM`, so with
`TERM=xterm-256color` the current Python cpuview renders in **256 colours** over SSH, and
every gradient is quantised. Until the probe exists, this on the server fixes it for
Windows Terminal users:

```sh
echo 'export COLORTERM=truecolor' >> ~/.bashrc     # or: COLORTERM=truecolor cpuview
```

The Rust version should use the DECRQSS echo above, and treat a probed truecolor terminal
as truecolor whatever TERM says.

## Rendering the half-block background (`blocks`)

The glass model and the per-cell rules are specified in `themes/README.md` ("Surfaces and the
glass model"). Implementation notes from prototyping against Textual 8 (they carry over to
any TUI stack):

- **Where to hook.** Composite at the **final frame** level, after layout, when every cell
  has its character, colours and the *surface* it belongs to. Doing it per widget means every
  widget needs its own screen offset, and scrolled or overlapping content gets it wrong. In
  Textual the single choke point is `App._display()`, which receives the compositor's
  `LayoutUpdate`/`ChopsUpdate`. Panels mark their cells by background colour (each surface
  colour of the palette is unique), and the compositor maps colour → surface. A modal with a
  fully transparent screen background passes the dashboard's styles through unchanged, so
  the menu can preview themes live on top of a correctly composited dashboard. A translucent
  modal background would blend the colours and break the colour → surface mapping.
- **Cache per size.** Canvas pixels, per-surface glass colours for blank cells (two colours
  per cell), and the style objects built from them are computed once per
  (theme or image, size, options). The per-frame work is a lookup per cell.
- **Front-buffer diff.** Keep a grid of what was last sent (char, fg, bg) and emit only cells
  that changed, with a cursor jump over unchanged runs longer than about 4 cells. This is
  needed over SSH: a wallpapered cell costs about 40 bytes (`ESC[38;2;r;g;b;48;2;r;g;bm▀`),
  so a full 172×40 frame is about 250 KB, while a typical live update changes only bars,
  numbers and the graph edge. It is also what makes `sixel`/`iterm` patches survive (next
  section). Invalidate the buffer on start, on resize, and after resuming from suspend.
- **256 colours.** Dither the *composited* colours (after glass), not the source image:
  4×4 Bayer offset ±(Bayer−7.5)·3.2 per channel, then the nearest xterm-256 entry (6×6×6
  cube + grey ramp). Dithering the source would be undone by the glass blend.
- **Selection and copy.** Blank cells become `▀`, so a terminal-native mouse selection copies
  `▀` characters. The app's own copy (if any) should use the logical text.

## Pixel modes: coexisting with a redrawing TUI

**Never re-send image data on a normal frame.** Transmit on start, resize and theme or option
change only. Everything else is placement (kitty) or small patches (sixel/iterm) driven by the
front-buffer diff.

### kitty (true background layer)

- Render the picture at the terminal's pixel size (`cols·cw × rows·ch`, cover-crop), PNG
  (`f=100`), and transmit once with `a=t,i=<id>,q=2` in 4096-byte base64 chunks (`m=1` …
  `m=0`). `q=2` suppresses replies so nothing lands in the TUI's input.
- Place it at the top-left with `a=p,i=<id>,p=1,c=<cols>,r=<rows>,z=-1073741826,C=1,q=2`. A
  z-index below −1,073,741,824 draws the image **under cells with a non-default background**,
  so opaque UI (badges, bar fills) stays on top.
- Surface cells are emitted with the **default** background (no SGR 48). The image shows
  through them and text is drawn on top.
- Tint: transmit one **1×1 RGBA pixel** per surface (tint colour, alpha = surface alpha) and
  place it scaled over each surface rectangle (`c=w,r=h`) at z = −1,073,741,825. The terminal
  blends it over the picture. Rectangles come from the frame's surface map: per-row runs merged
  vertically, usually 10–20 rectangles. Re-place only when the surface map changes (layout
  toggle, selection move, menu open). Placements are tiny commands; the image isn't
  re-sent.
- Readability of glyphs over a bright patch relies on the tint (there is no per-cell plate).
  Use the image alpha as `text_alpha` for panels.
- Clean-up: `a=d,d=I,i=<id>` for each id on resize (then re-transmit at the new size),
  theme change and exit. `a=d,d=A` as a last resort on exit. Some terminals drop images on
  `ED 2` (clear screen), so re-place after any full clear.

### sixel (the Windows Terminal path)

Sixel has no z-order. What Windows Terminal does (microsoft/terminal source, v1.22 to 1.26;
the same in other sixel terminals unless noted):

- **Any write to a cell erases the image pixels in that cell**, a space with the default
  background included (`TextBuffer::Replace` → `ImageSlice::EraseCells`); ECH/EL/ED and other
  fills too. A cell shows either pixels or a character, never both.
- **Images are painted over the text** and never erase it (`renderer.cpp`: "Paint any image
  content on top of the text"). Unset pixels (P2 = 1) keep whatever was there, earlier images
  included, so several images compose.
- **A fixed 10×20 px virtual cell** whatever the font: `CSI 16 t` answers 10×20 and `CSI 14 t`
  answers cols·10 × rows·20; each row's image slice is scaled to the real cell with linear
  filtering. A 128×45 window is a 1280×900 canvas; the SSH ioctl pixel size (often 640×480)
  is meaningless. XTSMGRAPHICS is not implemented.
- **256 colour registers**, not reset between images, but each image's colour numbers map
  afresh: every image must define its colours. RGB 0..100 per channel.
- **Scrolling mode** (DECSDM reset, the default): the image starts at the cursor; it scrolls
  the screen once a 6-px band doesn't fit. `?80h` puts images at the top-left and never
  scrolls.

So, per frame:

1. **Full repaint** (start, resize, theme change): `ED 2`, then **one picture for the whole
   window** (rows 0 … h-3: the last two rows never get pixels, so a band can't scroll the
   screen), then the text. Writing the text erases the picture under each character; every
   other cell keeps its pixels and is never written. When surfaces have different glass
   (the built-in themes' darker panels), one picture per glass, each covering only its own
   cells (the rest unset), composed by P2 = 1.
2. **Cells that turn blank** get their block-glyph mosaic at once and a **pixel patch** (one
   per run of adjacent cells in a row) when they are quiet: in eco in the same flush; in smooth
   at the next patch tick (4 a second) after a full tick blank. Cells of busy areas (graph
   interiors, the per-core heat map, bars and their tracks: `NOPATCH` / `TRACK`) wait three
   ticks, at most 96 a tick (eco: one sample, at most 128), so a moving graph line or bar end
   never costs more than a few patches.
3. **Ink glyphs**: panel frames (`─│╭╮╰╯├┤┬┴┼┃━╵`), dots (`·`) and braille grid lines, bars
   (`▁`…`█`, the lower k/8 of the cell in the bar's colour) and their tracks (a thin line at the
   base) are drawn into the patches themselves, so the picture continues through and above
   them. Each picture or patch defines its own ink colours as palette entries 190..254 (the
   picture is quantised to 190 colours; 255 = unset); a cell whose colour finds no free entry is
   written as text instead. Anti-aliased bar ends (blends) stay text.
4. **Text cells**: background = the picture's average there, through the same veil as the
   blank cells. The light glass first dims the picture (`image::tone`: luminance through a soft
   knee `L / (1 + (L/0.07)³)^⅓`, colour kept, +15 % saturation, yellows leaning away from
   olive), and the palette is made 4.5:1 (text) / 3:1 (frames, bars) against a grey at that
   cap (`theme::guarantee`), so the colours read everywhere as designed. The compositor still
   checks every cell against what is behind it (`color::lighten_to`: lighter in eighths, same
   hue, never darker; inks in patches too), judged for text on the picture ±3 cells around as
   well, so a word changes together. Only where even white can't reach 4.5:1 (clear glass on a
   bright picture) does the cell get a darker plate, and a single blank between two plated
   words takes the plate, so a label reads as one.
5. The hi-res picture is stored at the screen's pixel size, but no larger than the picture
   itself (a 1080p picture in a 4K window is kept at 1080p and scaled up per patch).

Fit: `cover` (default) fills the window, cropping around the focus: `image.<file>.focus` if
set, else the bundled wallpaper's (`wallpapers/credits.toml` `focus = [x, y]`), else
`image::auto_focus` (once at load: on a 48-px-wide copy, the centroid of edge energy +
contrast with the mean + colourfulness, squared, times a centre prior of σ 0.3, pulled a
quarter back to the middle). `contain` shows the whole picture; the margins are a cover-crop
of the picture at 1/24 scale, blurred and dimmed to 70 %, enlarged, with the picture's edges
feathered over 3 % of its short side.

**tmux** removes an image when text overwrites it, so inside tmux the full repaint sends
patches per run of blank cells instead of one picture.

### iTerm2 inline images

Same patch strategy as sixel, with PNG patches via
`OSC 1337 ; File=inline=1;size=N;width=<cells>;height=1;preserveAspectRatio=0;doNotMoveCursor=1 : <base64> BEL`.
iTerm2 replaces the cells under an inline image, so patches must cover **only** blank cells
(runs, never spanning text). `doNotMoveCursor=1` (iTerm2 ≥ 3.5, WezTerm) avoids scrolling.

### tmux and screen

- tmux ≥ 3.4 built with sixel answers DA1 with `4` and handles sixel itself: use plain sixel.
- kitty / iTerm2 sequences need passthrough: `ESC P tmux; <sequence with every ESC doubled> ESC \`,
  which requires `set -g allow-passthrough on` in `~/.tmux.conf`. tmux doesn't track
  passthrough images: they don't follow pane switches, and kitty needs Unicode placeholders
  to survive. **Auto mode therefore never picks kitty or iterm inside tmux** (sixel or blocks
  instead). An explicit `--image-mode kitty` inside tmux wraps everything, for single-pane
  use.
- GNU screen: no graphics passthrough. Graphics modes downgrade to `blocks`.

### terminal (transparent) mode, e.g. Windows Terminal background image

cpuview emits every surface cell with the terminal's **default** background (and a palette made
for unknown backgrounds: the `transparent` theme, or an image palette when the user points
cpuview at the same picture with `--image`). Windows Terminal then shows its profile's
background image at full resolution behind all text.

Windows Terminal → Settings → Open JSON file (`settings.json`), add a profile:

```json
{
  "profiles": {
    "list": [
      {
        "name": "cpuview",
        "commandline": "ssh -t you@yourserver cpuview --theme transparent",
        "backgroundImage": "C:\\Users\\you\\Pictures\\cpuview\\nasa-iss-aurora.jpg",
        "backgroundImageStretchMode": "uniformToFill",
        "backgroundImageAlignment": "center",
        "backgroundImageOpacity": 0.45,
        "background": "#05070d",
        "font": { "face": "Cascadia Mono" }
      }
    ]
  }
}
```

`backgroundImageOpacity` (0–1) blends the picture over `background`. 0.3–0.5 keeps text
readable. The same keys can be set on an existing profile or under `profiles.defaults`.
`-t` makes ssh allocate a terminal.

Getting a picture onto the Windows side, in PowerShell (OpenSSH ships with Windows 10/11):

```powershell
mkdir $env:USERPROFILE\Pictures\cpuview -Force
scp you@yourserver:~/.config/cpuview/images/nasa-iss-aurora.jpg $env:USERPROFILE\Pictures\cpuview\
# or a bundled wallpaper from the repo:
scp you@yourserver:~/cpuview/wallpapers/lofi-night-city.png $env:USERPROFILE\Pictures\cpuview\
```

## Getting images in

- `cpuview --image PATH` or `--image https://…/pic.jpg`: copied (or downloaded) into
  `~/.config/cpuview/images/`, made the active theme and remembered.
- **Drop folder**: any png/jpg/jpeg/webp/gif in `~/.config/cpuview/images/` appears in the
  theme menu. From Windows PowerShell:
  `scp .\pic.jpg you@yourserver:~/.config/cpuview/images/`
  (from Linux/macOS: `scp pic.jpg yourserver:~/.config/cpuview/images/`).
- Theme menu → "Add image…": a path or URL field, plus a file browser limited to images.
- The bundled `wallpapers/` pack (credits in `wallpapers/CREDITS.md`) is listed as well.

**Safe downloads**: http/https only; 15 s total timeout; 25 MB cap, enforced while streaming
and not taken from `Content-Length`; write to a temp file in the images folder; decode with the
image library (`verify()` plus a real load of the first frame) before keeping it; file name from
the URL path reduced to `[A-Za-z0-9._-]`, extension taken from the *decoded* format, and
deduplicated. Nothing is executed or interpreted. SVG, archives and HTML are rejected.
Animated GIF/WebP use the first frame.

**Without the image library** (Pillow in Python), image themes are disabled. The menu shows
"image themes need Pillow: pip install pillow" and procedural themes keep working, since they
need no image code.

## Config

`~/.config/cpuview/config` (honour `XDG_CONFIG_HOME`), plain `key = value` lines:

```
theme = nebula                     # or image:nasa-iss-aurora.jpg
image_mode = auto
sort = core
view = auto
image.nasa-iss-aurora.jpg.dim = 0.9    # wallpaper brightness 0.2–1.0
image.nasa-iss-aurora.jpg.blur = 0     # extra blur, in cells (0 / 1 / 2 / 4)
```

## Palette from an image, and readability

Specified in `themes/README.md` ("Image themes" and "Surfaces and the glass model"): k-means
over a 96×96 thumbnail, the darkest well-populated colour becomes the tint, the two most vivid
distinct hues become the accents, and text colours are near-white tinted toward accent 1.
Readability comes from three things together: lower alpha on blank cells (the picture shows
where there is no data), higher `text_alpha` behind glyphs, and the **contrast guard**, which
raises the plate alpha cell by cell until text reaches 4.5:1 or the most the tint allows.
Tested against all 8 bundled wallpapers. The Webb "Cosmic Cliffs" (bright orange cliffs behind
the core grid) and the pixel night city (busy lit windows) were the stress cases. Both stay
legible with the guard and were not before it.

## Status (Rust implementation, `src/`)

| item | status |
|---|---|
| Theme data, background generators, glass compositor, image palette | implemented (`src/bg.rs`, `src/screen.rs`, `src/image.rs`); every built-in background matches `themes/reference.py` pixel for pixel at 60×20 (one ±1 rounding difference in `mono`), glyph stars identical |
| Probe parsing and mode selection | implemented (`src/probe.rs`); the 13 recorded reply shapes plus the edge cases of `termgfx.py selftest` are unit tests; end to end through a pseudo-terminal with fake Windows Terminal / kitty / xterm / silent replies |
| Sixel encoder | implemented (`src/gfx.rs`); round-trip tested with a decoder (1×1 … 300×20, transparency, RLE, exact bytes) |
| Sixel patches on blank cells | implemented: front-buffer diff, patches only for cells that became blank, never on the last two rows, never in graph areas (cells that change every second stay half-blocks); verified headlessly by decoding the patches into an overlay where text writes erase image cells (Windows Terminal semantics) |
| kitty / iTerm2 | implemented (kitty: picture under the text, blurred panel crops at ¼ resolution, 1×1 tint overlays; iTerm2: PNG patches); escape format unit-tested, **not viewed on a real terminal** |
| tmux passthrough | implemented for kitty / iTerm2 sequences (auto never picks them inside tmux) |
| Windows Terminal transparent mode | `--theme transparent` or `--image-mode terminal`; needs the user's terminal to confirm |

Needs real-terminal confirmation: run `cpuview --doctor` in Windows Terminal (expected:
`auto → sixel`, truecolor via DECRQSS) and look at the test strip it draws.

Differences from the design above: the config file is `~/.config/cpuview/config` with
`key = value` lines (no TOML parser in the binary), and panels draw a **blurred** copy of the
picture (radius 2 cells + the user's blur) with blank/text alpha 0.52/0.6, which keeps busy
pictures (pixel night city) calm behind the data while the bare screen keeps the sharp image.

#!/usr/bin/env python3
"""Reference terminal-graphics helpers for cpuview image themes (spec: docs/image-themes.md).

Standalone, standard library only (Pillow optional, only for loading a picture in `demo`):

  python3 termgfx.py doctor            probe the terminal you are in and explain the choice
  python3 termgfx.py demo [MODE] [IMG] draw a test image with sixel / kitty / iterm / blocks
  python3 termgfx.py selftest          unit tests: reply parsing, mode matrix, sixel round trip

Contents
  probe()          send all capability queries at once, read replies until the DA1 sentinel
  parse_replies()  bytes -> Caps
  choose_mode()    Caps + env + user override -> (mode, reason)
  tmux_wrap()      DCS passthrough for graphics inside tmux
  sixel_encode()   palette-indexed pixels -> DECSIXEL (RLE, transparent background)
  sixel_decode()   minimal decoder, used by the self-test to verify the encoder
  kitty_*()        kitty graphics protocol: chunked transmit, placement, delete
  iterm_inline()   iTerm2 inline image (OSC 1337)
"""
from __future__ import annotations

import base64
import os
import re
import select
import sys
import time
from dataclasses import dataclass, field

ESC = "\x1b"
ST = ESC + "\\"

# ── probing ───────────────────────────────────────────────────────────────────

KITTY_QUERY = f"{ESC}_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA{ST}"
QUERIES = (
    f"{ESC}[>0q"                 # XTVERSION  -> DCS > | name(version) ST
    + KITTY_QUERY                # kitty graphics -> APC G i=31;OK ST
    + f"{ESC}[16t"               # cell size px   -> CSI 6 ; h ; w t
    + f"{ESC}[14t"               # text area px   -> CSI 4 ; h ; w t
    + f"{ESC}[48;2;1;2;3m"       # set a truecolor background ...
    + f"{ESC}P$qm{ST}"           # ... DECRQSS SGR -> DCS 1 $ r <sgr> m ST (echoes 48:2/48;2 if kept)
    + f"{ESC}[0m"
    + f"{ESC}[c"                 # DA1 sentinel   -> CSI ? 6x ; 4 ; ... c   (every terminal answers)
)


@dataclass
class Caps:
    name: str = ""               # from XTVERSION, e.g. "kitty(0.36.4)", "WezTerm 20240203", "tmux 3.4"
    kitty: bool = False
    sixel: bool = False
    da1: list = field(default_factory=list)
    cell_px: tuple | None = None       # (w, h)
    window_px: tuple | None = None     # (w, h)
    truecolor_echo: bool = False       # DECRQSS returned our 48;2;1;2;3
    answered: bool = False             # DA1 seen (the terminal talks back at all)
    raw: bytes = b""


def probe(timeout=0.3, fd=None):
    """Query the controlling terminal. Writes all queries at once and reads until the DA1
    reply (or `timeout` seconds). Uses the alternate screen so stray text from a terminal
    that echoes an unknown sequence never lands in the user's scrollback, then restores the
    tty and discards any late bytes (tcflush) so nothing leaks into the app's input."""
    import termios
    import tty
    own = fd is None
    if own:
        try:
            fd = os.open("/dev/tty", os.O_RDWR | os.O_NOCTTY)
        except OSError:
            return Caps()
    old = termios.tcgetattr(fd)
    buf = b""
    try:
        tty.setcbreak(fd, termios.TCSANOW)
        new = termios.tcgetattr(fd)
        new[3] &= ~termios.ECHO
        termios.tcsetattr(fd, termios.TCSANOW, new)
        q = QUERIES
        if os.environ.get("TMUX"):   # the kitty query must reach the outer terminal
            q = q.replace(KITTY_QUERY, tmux_wrap(KITTY_QUERY))
        os.write(fd, (f"{ESC}[?1049h" + q).encode())
        t_start = time.monotonic()
        end = t_start + timeout
        while time.monotonic() < end:
            r, _, _ = select.select([fd], [], [], max(0.0, end - time.monotonic()))
            if not r:
                break
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            buf += chunk
            if re.search(rb"\x1b\[\?[\d;]*c", buf):
                break
            # replies have started (slow SSH link): allow up to 1 s for the DA1 sentinel
            end = max(end, t_start + 1.0)
    finally:
        os.write(fd, f"{ESC}[?1049l".encode())
        time.sleep(0.02)
        termios.tcflush(fd, termios.TCIFLUSH)
        termios.tcsetattr(fd, termios.TCSANOW, old)
        if own:
            os.close(fd)
    return parse_replies(buf)


def parse_replies(buf: bytes) -> Caps:
    c = Caps(raw=buf)
    if m := re.search(rb"\x1bP>\|([^\x1b]*)\x1b\\", buf):
        c.name = m.group(1).decode(errors="replace")
    if m := re.search(rb"\x1b_Gi=31;([^\x1b]*)\x1b\\", buf):
        c.kitty = m.group(1).startswith(b"OK")
    if m := re.search(rb"\x1b\[6;(\d+);(\d+)t", buf):
        h, w = int(m.group(1)), int(m.group(2))
        c.cell_px = (w, h) if w and h else None
    if m := re.search(rb"\x1b\[4;(\d+);(\d+)t", buf):
        h, w = int(m.group(1)), int(m.group(2))
        c.window_px = (w, h) if w and h else None
    if m := re.search(rb"\x1bP1\$r([^\x1b]*)\x1b\\", buf):
        c.truecolor_echo = bool(re.search(rb"48[:;]2[:;]+1[:;]2[:;]3", m.group(1)))
    if m := re.search(rb"\x1b\[\?([\d;]*)c", buf):
        c.answered = True
        c.da1 = [int(p) for p in m.group(1).split(b";") if p]
        c.sixel = 4 in c.da1[1:]
    return c


# terminals known to draw kitty images with z < 0 *under* text (true background layer)
KITTY_UNDER_TEXT = ("kitty", "ghostty", "wezterm")
ITERM_INLINE = ("iterm2", "wezterm", "mintty", "vscode")
MODERN = ("kitty", "ghostty", "wezterm", "foot", "konsole", "iterm2", "alacritty", "contour",
          "mintty", "xterm", "vscode", "tabby", "rio", "windows terminal", "tmux")
MODES = ("auto", "kitty", "sixel", "iterm", "blocks", "blocks256", "terminal", "off")


def cell_size(caps, cols=None, rows=None, xpixel=0, ypixel=0):
    """Cell size in pixels: CSI 16 t, else CSI 14 t / grid, else TIOCGWINSZ pixels / grid,
    else 10x20 when sixel is available (the VT340 cell that Windows Terminal also uses
    for sixel scaling), else None."""
    if caps.cell_px:
        return caps.cell_px
    if caps.window_px and cols and rows:
        return caps.window_px[0] // cols, caps.window_px[1] // rows
    if xpixel and ypixel and cols and rows:
        return xpixel // cols, ypixel // rows
    return (10, 20) if caps.sixel else None


def choose_mode(caps: Caps, env=None, requested="auto"):
    """Pick an image mode. Returns (mode, reason). Order for auto:
    kitty (under-text layering) -> sixel -> iterm -> blocks (truecolor) -> blocks256 -> off.
    `terminal` (transparent cells, the terminal's own background image) is never automatic."""
    env = os.environ if env is None else env
    name = caps.name.lower()
    in_tmux = bool(env.get("TMUX")) or name.startswith("tmux")
    in_screen = env.get("TERM", "").startswith("screen") and not in_tmux
    colorterm = env.get("COLORTERM", "").lower() in ("truecolor", "24bit")
    truecolor = colorterm or caps.truecolor_echo or any(n in name for n in MODERN)
    c256 = truecolor or "256color" in env.get("TERM", "")
    if requested != "auto":
        if requested in ("kitty", "sixel", "iterm") and in_screen:
            return "blocks" if truecolor else "blocks256", f"{requested} is not possible inside GNU screen"
        return requested, "requested explicitly"
    if not caps.answered:
        if truecolor:
            return "blocks", "terminal did not answer queries; COLORTERM says truecolor"
        return ("blocks256", "no replies; TERM says 256 colours") if c256 else ("off", "no replies, basic TERM")
    if caps.kitty and not in_tmux and any(n in name for n in KITTY_UNDER_TEXT):
        return "kitty", f"kitty graphics OK and {caps.name or 'terminal'} layers images under text"
    if caps.sixel:
        why = "DA1 lists 4 (sixel)" + (" — tmux renders sixel itself" if in_tmux else "")
        return "sixel", why
    if caps.kitty and not in_tmux:
        return "kitty", "kitty graphics OK (no sixel)"
    if any(n in name for n in ITERM_INLINE) and not in_tmux:
        return "iterm", f"{caps.name} supports iTerm2 inline images"
    if truecolor:
        src = "COLORTERM" if colorterm else "DECRQSS echo" if caps.truecolor_echo else caps.name
        return "blocks", f"no pixel graphics; truecolor via {src}"
    if c256:
        return "blocks256", "no pixel graphics; 256 colours"
    return "off", "no graphics and fewer than 256 colours"


def tmux_wrap(seq: str) -> str:
    """tmux passthrough (needs `set -g allow-passthrough on`): DCS tmux; <seq, ESC doubled> ST."""
    return f"{ESC}Ptmux;" + seq.replace(ESC, ESC + ESC) + ST


# ── sixel ─────────────────────────────────────────────────────────────────────

_ADD63 = bytes(min(255, 63 + i) for i in range(256))
_RLE = re.compile(rb"(.)\1{3,}")


def _rle(m):
    return b"!%d" % len(m.group(0)) + m.group(1)


def sixel_encode(idx: bytes, w: int, h: int, palette, transparent=None) -> bytes:
    """Encode palette-indexed pixels (len(idx) == w*h, values < len(palette) <= 256).
    palette: [(r, g, b)] 0..255. Pixels equal to `transparent` are left unset, and P2=1
    keeps unset pixels transparent, which also covers the padding below the last pixel row
    when h is not a multiple of 6. Colours are defined only if used.

    Per six-row band and per colour present, the 6 rows are mapped to bit masks with
    bytes.translate (colour -> 1<<k, others -> 0), OR-ed together as big integers (no
    carries: every row owns a different bit), shifted to sixel characters (+63), trailing
    empty sixels dropped and runs of 4+ run-length encoded as !<n><char>."""
    used = sorted(set(idx) - ({transparent} if transparent is not None else set()))
    out = [b"\x1bP0;1;0q", b'"1;1;%d;%d' % (w, h)]
    for c in used:
        r, g, b = palette[c]
        out.append(b"#%d;2;%d;%d;%d" % (c, round(r * 100 / 255), round(g * 100 / 255), round(b * 100 / 255)))
    zero = bytearray(256)
    for y0 in range(0, h, 6):
        rows = [idx[(y0 + k) * w:(y0 + k + 1) * w] for k in range(min(6, h - y0))]
        present = set().union(*(set(r) for r in rows)) - ({transparent} if transparent is not None else set())
        lines = []
        for c in sorted(present):
            acc = 0
            for k, row in enumerate(rows):
                t = zero[:]
                t[c] = 1 << k
                acc |= int.from_bytes(row.translate(bytes(t)), "big")
            line = acc.to_bytes(w, "big").translate(_ADD63).rstrip(b"?")
            if line:
                lines.append(b"#%d" % c + _RLE.sub(_rle, line))
        out.append(b"$".join(lines) + (b"-" if y0 + 6 < h else b""))
    out.append(b"\x1b\\")
    return b"".join(out)


def sixel_decode(data: bytes):
    """Minimal DECSIXEL decoder -> (w, h, pixels) with pixels[y][x] = colour index or None.
    Handles raster attributes, colour selection/definition, RLE, $ and -."""
    body = data[data.index(b"q") + 1:data.rindex(b"\x1b\\")]
    m = re.match(rb'"(\d+);(\d+);(\d+);(\d+)', body)
    w, h = int(m.group(3)), int(m.group(4))
    body = body[m.end():]
    px = [[None] * w for _ in range(h)]
    x = band = col = 0
    i = 0
    while i < len(body):
        ch = body[i:i + 1]
        if ch == b"#":
            m = re.match(rb"#(\d+)(;2;\d+;\d+;\d+)?", body[i:])
            if not m.group(2):
                col = int(m.group(1))
            i += m.end()
            continue
        if ch == b"$":
            x = 0
        elif ch == b"-":
            x, band = 0, band + 1
        elif ch == b"!":
            m = re.match(rb"!(\d+)(.)", body[i:], re.S)
            n, s = int(m.group(1)), m.group(2)[0] - 63
            for _ in range(n):
                for k in range(6):
                    if s >> k & 1 and band * 6 + k < h:
                        px[band * 6 + k][x] = col
                x += 1
            i += m.end()
            continue
        elif 63 <= ch[0] <= 126:
            s = ch[0] - 63
            for k in range(6):
                if s >> k & 1 and band * 6 + k < h:
                    px[band * 6 + k][x] = col
            x += 1
        i += 1
    return w, h, px


def quantize_cube(rgb: bytes, w: int, h: int):
    """Tiny stdlib quantiser (6x6x6 cube + ordered dither) for when Pillow is absent.
    Returns (indices, palette). With Pillow use Image.quantize(256, dither=FLOYDSTEINBERG)."""
    bayer = (0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5)
    lv = (0, 51, 102, 153, 204, 255)
    pal = [(r, g, b) for r in lv for g in lv for b in lv]
    out = bytearray(w * h)
    for y in range(h):
        for x in range(w):
            o = (y * w + x) * 3
            d = (bayer[(y & 3) * 4 + (x & 3)] - 7.5) * 3.2
            q = [min(5, max(0, round((rgb[o + k] + d) / 51))) for k in range(3)]
            out[y * w + x] = q[0] * 36 + q[1] * 6 + q[2]
    return bytes(out), pal


# ── kitty ─────────────────────────────────────────────────────────────────────


def kitty_transmit(png: bytes, image_id: int, chunk=4096) -> str:
    """Transmit PNG data (f=100) as image `image_id`, base64 in <=4096-byte chunks,
    q=2 so the terminal sends no replies into the app's input."""
    b64 = base64.standard_b64encode(png).decode()
    parts = [b64[i:i + chunk] for i in range(0, len(b64), chunk)] or [""]
    out = []
    for n, p in enumerate(parts):
        more = 1 if n < len(parts) - 1 else 0
        ctl = f"a=t,f=100,t=d,i={image_id},q=2,m={more}" if n == 0 else f"m={more}"
        out.append(f"{ESC}_G{ctl};{p}{ST}")
    return "".join(out)


def kitty_place(image_id, placement_id, cols, rows, z=-1, x=None, y=None) -> str:
    """Place at the cursor (or move to 0-based x, y first) scaled to cols x rows cells.
    C=1 keeps the cursor still. z < 0 draws under text; z < -1073741824 also under cells
    that have a non-default background colour."""
    pos = f"{ESC}[{y + 1};{x + 1}H" if x is not None else ""
    return pos + f"{ESC}_Ga=p,i={image_id},p={placement_id},c={cols},r={rows},z={z},C=1,q=2;{ST}"


def kitty_delete(image_id=None) -> str:
    """Delete one image and free its data (d=I), or all placements and data (d=A)."""
    return f"{ESC}_Ga=d,d=I,i={image_id},q=2;{ST}" if image_id else f"{ESC}_Ga=d,d=A,q=2;{ST}"


# ── iTerm2 ────────────────────────────────────────────────────────────────────


def iterm_inline(png: bytes, cols: int, rows: int) -> str:
    """OSC 1337 inline image stretched to exactly cols x rows cells; doNotMoveCursor=1
    (iTerm2 >= 3.5, WezTerm) avoids a scroll when drawing near the bottom row."""
    b64 = base64.standard_b64encode(png).decode()
    return (f"{ESC}]1337;File=inline=1;size={len(png)};width={cols};height={rows};"
            f"preserveAspectRatio=0;doNotMoveCursor=1:{b64}\a")


# ── doctor / demo ─────────────────────────────────────────────────────────────


def doctor():
    import shutil
    import struct
    cols, rows = shutil.get_terminal_size()
    xp = yp = 0
    try:
        import fcntl
        import termios
        r = fcntl.ioctl(sys.stdout.fileno(), termios.TIOCGWINSZ, b"\0" * 8)
        _, _, xp, yp = struct.unpack("HHHH", r)
    except OSError:
        pass
    t0 = time.monotonic()
    caps = probe()
    dt = (time.monotonic() - t0) * 1000
    mode, why = choose_mode(caps)
    cell = cell_size(caps, cols, rows, xp, yp)
    print(f"terminal      {caps.name or '(no XTVERSION reply)'}")
    print(f"TERM          {os.environ.get('TERM', '')}   COLORTERM={os.environ.get('COLORTERM', '') or '(unset)'}"
          f"   ssh={'yes' if os.environ.get('SSH_CONNECTION') else 'no'}")
    print(f"replies       {'yes' if caps.answered else 'NO'} in {dt:.0f} ms   DA1={caps.da1}")
    print(f"truecolor     {'yes' if caps.truecolor_echo else 'no echo'} (DECRQSS)")
    print(f"sixel         {'yes' if caps.sixel else 'no'}")
    print(f"kitty         {'yes' if caps.kitty else 'no'}")
    print(f"cell size     {cell[0]}x{cell[1]} px" if cell else "cell size     unknown")
    print(f"window        {cols}x{rows} cells, ioctl pixels {xp}x{yp}, CSI 14t {caps.window_px}")
    print(f"tmux          {'yes' if os.environ.get('TMUX') or caps.name.lower().startswith('tmux') else 'no'}")
    print(f"→ image mode  {mode}  ({why})")
    print(f"raw replies   {caps.raw!r}")
    return mode


def demo(mode=None, path=None):
    mode = mode or doctor()
    w, h = 320, 120
    if path:
        from PIL import Image, ImageOps
        im = ImageOps.fit(Image.open(path).convert("RGB"), (w, h))
        rgb = im.tobytes()
    else:  # a hue sweep with a brightness ramp
        import colorsys
        rgb = bytearray()
        for y in range(h):
            for x in range(w):
                r, g, b = colorsys.hsv_to_rgb(x / w, 0.8, 0.25 + 0.75 * (1 - y / h))
                rgb += bytes((round(r * 255), round(g * 255), round(b * 255)))
        rgb = bytes(rgb)
    print(f"\n{mode} test image below (if you see a colourful strip, it works):")
    if mode == "sixel":
        idx, pal = quantize_cube(rgb, w, h)
        sys.stdout.write(sixel_encode(idx, w, h, pal).decode("latin-1") + "\n")
    elif mode in ("kitty", "iterm"):
        import zlib
        raw = b"".join(b"\0" + rgb[y * w * 3:(y + 1) * w * 3] for y in range(h))

        def chunk(t, d):
            c = t + d
            return len(d).to_bytes(4, "big") + c + zlib.crc32(c).to_bytes(4, "big")
        png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", w.to_bytes(4, "big") + h.to_bytes(4, "big") + b"\x08\x02\0\0\0")
               + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))
        if mode == "kitty":
            sys.stdout.write(kitty_transmit(png, 77) + kitty_place(77, 1, 32, 6, z=0) + "\n" * 6)
        else:
            sys.stdout.write(iterm_inline(png, 32, 6) + "\n" * 6)
    else:
        for y in range(0, 12, 2):
            line = []
            for x in range(0, 64):
                o1 = ((y * h // 12) * w + x * w // 64) * 3
                o2 = (((y + 1) * h // 12) * w + x * w // 64) * 3
                a, b = rgb[o1:o1 + 3], rgb[o2:o2 + 3]
                line.append(f"\x1b[38;2;{a[0]};{a[1]};{a[2]};48;2;{b[0]};{b[1]};{b[2]}m▀")
            print("".join(line) + "\x1b[0m")
    sys.stdout.flush()


# ── self-test ─────────────────────────────────────────────────────────────────

SAMPLES = {   # recorded / documented reply shapes (DA1 last, as terminals answer in order)
    "kitty": (b"\x1bP>|kitty(0.36.4)\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[6;21;10t\x1b[4;840;1700t"
              b"\x1bP1$r0;48:2:1:2:3m\x1b\\\x1b[?62;c", "kitty"),
    "windows-terminal-1.22": (b"\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c", "sixel"),
    "conhost (DA1 only)": (b"\x1b[?1;0c", "blocks256"),
    "windows-terminal-1.21": (b"\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?61;6;7;14;21;22;23;24;28;32;42c", "blocks"),
    "foot": (b"\x1bP>|foot(1.18.1)\x1b\\\x1b[6;22;11t\x1bP1$r0;48:2::1:2:3m\x1b\\\x1b[?62;4;22c", "sixel"),
    "wezterm": (b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t"
                b"\x1b[?65;4;6;18;22c", "kitty"),
    "konsole": (b"\x1bP>|Konsole 24.02.1\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c", "sixel"),
    "iterm2": (b"\x1bP>|iTerm2 3.5.0\x1b\\\x1b[?62;22;52c", "iterm"),
    "tmux-no-sixel": (b"\x1bP>|tmux 3.4\x1b\\\x1b[?1;2c", "blocks"),
    "tmux-sixel": (b"\x1bP>|tmux 3.4\x1b\\\x1b[?1;2;4c", "sixel"),
    "xterm-vt340": (b"\x1bP>|XTerm(390)\x1b\\\x1b[?63;1;2;4;6;9;15;16;22;28c", "sixel"),
    "xterm-default": (b"\x1bP>|XTerm(390)\x1b\\\x1b[?65;1;9c", "blocks"),
    "silent": (b"", "blocks256"),
}


def selftest():
    import random
    ok = True
    env = {"TERM": "xterm-256color"}
    for name, (buf, want) in SAMPLES.items():
        caps = parse_replies(buf)
        e = dict(env, TMUX="/tmp/tmux-1000/default,1,0") if name.startswith("tmux") else env
        got, why = choose_mode(caps, e)
        flag = "ok " if got == want else "BAD"
        ok &= got == want
        print(f"{flag} {name:24} -> {got:9} {why}")
    c = parse_replies(SAMPLES["kitty"][0])
    assert c.cell_px == (10, 21) and c.window_px == (1700, 840) and c.truecolor_echo, c
    assert cell_size(parse_replies(SAMPLES["windows-terminal-1.22"][0])) == (10, 20)
    assert choose_mode(Caps(), {"TERM": "xterm-256color", "COLORTERM": "truecolor"})[0] == "blocks"
    assert choose_mode(Caps(answered=True), {"TERM": "vt100"})[0] == "off"
    assert choose_mode(Caps(answered=True, sixel=True), {"TERM": "screen"}, "sixel")[0] != "sixel"
    assert tmux_wrap("\x1b_Ga=d\x1b\\") == "\x1bPtmux;\x1b\x1b_Ga=d\x1b\x1b\\\x1b\\"
    # sixel round trip: random palette image, odd sizes, transparency, long runs
    rnd = random.Random(1)
    for w, h in ((1, 1), (7, 5), (37, 13), (64, 20), (200, 6)):
        pal = [(rnd.randrange(256), rnd.randrange(256), rnd.randrange(256)) for _ in range(20)]
        idx = bytes(rnd.choice([0, 0, 0, 3, 7, 19]) if rnd.random() < 0.7 else rnd.randrange(20)
                    for _ in range(w * h))
        for tr in (None, 0):
            data = sixel_encode(idx, w, h, pal, transparent=tr)
            dw, dh, px = sixel_decode(data)
            want = [[None if (tr is not None and idx[y * w + x] == tr) else idx[y * w + x]
                     for x in range(w)] for y in range(h)]
            good = (dw, dh) == (w, h) and px == want
            ok &= good
            print(f"{'ok ' if good else 'BAD'} sixel round trip {w}x{h} transparent={tr} ({len(data)} bytes)")
    kt = kitty_transmit(b"x" * 10000, 5)
    assert kt.count("\x1b_G") == 4 and "m=0;" in kt and kt.startswith("\x1b_Ga=t,f=100,t=d,i=5,q=2,m=1;")
    print("ok  kitty chunking")
    print("ALL OK" if ok else "FAILURES")
    return ok


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "doctor"
    if cmd == "selftest":
        sys.exit(0 if selftest() else 1)
    elif cmd == "demo":
        demo(*(sys.argv[2:4]))
    elif cmd == "doctor":
        doctor()
    else:
        print(__doc__)

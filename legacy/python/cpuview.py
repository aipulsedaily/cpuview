#!/usr/bin/env python3
"""cpuview — btop-style interactive per-core frequency / load / temperature monitor.

Keys:  arrows / hjkl  select core      enter / d  toggle detail panel
       s  cycle sort (core/load/freq)  g  toggle history graphs
       c  cycle view (auto/full/compact — auto fits every core on screen)
       + / -  update interval          space  pause      r  reset peaks
       q  quit                         mouse: click a core to select it
"""
import glob
import time
from collections import deque

from rich.text import Text
from textual import events
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Grid, Horizontal, VerticalScroll
from textual.css.query import NoMatches
from textual.widget import Widget

SYS = "/sys/devices/system/cpu"
HIST = 1000
INTERVALS = (0.25, 0.5, 1.0, 2.0, 5.0)
SORTS = ("core", "load", "freq")
VIEWS = ("auto", "full", "compact")
SPARK_SECONDS = 30   # time span of the per-thread load sparklines


# ── data ──────────────────────────────────────────────────────────────────────

def read(p, default=None):
    try:
        with open(p) as f:
            return f.read().strip()
    except OSError:
        return default


def parse_list(s):
    out = []
    for part in s.split(","):
        if "-" in part:
            a, b = map(int, part.split("-"))
            out += range(a, b + 1)
        elif part:
            out.append(int(part))
    return out


def cpu_int(c, name, default=0):
    try:
        return int(read(f"{SYS}/cpu{c}/{name}", default) or default)
    except ValueError:
        return default


CPUS = parse_list(read(f"{SYS}/online", "0"))
MAXF = max(cpu_int(c, "cpufreq/cpuinfo_max_freq") for c in CPUS) / 1000 or 1
MINF = min((cpu_int(c, "cpufreq/cpuinfo_min_freq") for c in CPUS), default=0) / 1000
BOOST = 0.9 * MAXF   # clocks at or above this count as boosting
MODEL = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo")
              if l.startswith("model name")), "CPU")
DRIVER = read(f"{SYS}/cpu0/cpufreq/scaling_driver", "")


def core_groups():
    seen, groups = set(), []
    for c in CPUS:
        if c in seen:
            continue
        ids = [i for i in parse_list(read(f"{SYS}/cpu{c}/topology/thread_siblings_list", str(c)))
               if i in CPUS]
        seen.update(ids)
        groups.append(ids)
    return groups


def l3_domains(groups):
    """Index of the L3 cache (≈ CCD on Ryzen) each core group belongs to."""
    keys = [read(f"{SYS}/cpu{g[0]}/cache/index3/shared_cpu_list", "") for g in groups]
    order = list(dict.fromkeys(keys))
    return [order.index(k) for k in keys]


def stat():
    d = {}
    for l in open("/proc/stat"):
        if l.startswith("cpu") and l[3].isdigit():
            f = l.split()
            v = list(map(int, f[1:]))
            d[int(f[0][3:])] = (v[3] + v[4], sum(v))
    return d


def temps():
    out = {}
    for h in glob.glob("/sys/class/hwmon/hwmon*"):
        if read(f"{h}/name") in ("k10temp", "zenpower", "coretemp"):
            for lab in sorted(glob.glob(f"{h}/temp*_label")):
                val = read(lab.replace("_label", "_input"))
                if val:
                    out[read(lab)] = int(val) / 1000
    return out


def uptime():
    s = int(float(read("/proc/uptime", "0 0").split()[0]))
    d, s = divmod(s, 86400)
    h, s = divmod(s, 3600)
    return f"{d}d {h}h {s // 60:02}m" if d else f"{h}h {s // 60:02}m"


def span(seconds):
    m, s = divmod(int(seconds), 60)
    return f"{m}m {s:02}s" if m else f"{s}s"


class Sampler:
    def __init__(self):
        self.groups = core_groups()
        self.ccd = l3_domains(self.groups)
        self.rank = [cpu_int(g[0], "cpufreq/amd_pstate_prefcore_ranking", -1) for g in self.groups]
        self.freq = {c: deque(maxlen=HIST) for c in CPUS}
        self.load = {c: deque(maxlen=HIST) for c in CPUS}
        self.avg_f = deque(maxlen=HIST)
        self.tot_u = deque(maxlen=HIST)
        self.peak = dict.fromkeys(CPUS, 0.0)
        self.temps = {}
        self.temp_hist = {}
        self.prev = stat()

    def sample(self):
        cur = stat()
        busy = total = 0
        for c in CPUS:
            u = 0.0
            if c in cur and c in self.prev:
                di = cur[c][0] - self.prev[c][0]
                dt = cur[c][1] - self.prev[c][1]
                if dt > 0:
                    u = max(0.0, min(1.0, 1 - di / dt))
                    busy += dt - di
                    total += dt
            f = int(read(f"{SYS}/cpu{c}/cpufreq/scaling_cur_freq", 0) or 0) / 1000
            self.load[c].append(u)
            self.freq[c].append(f)
            self.peak[c] = max(self.peak[c], f)
        self.prev = cur
        self.tot_u.append(busy / total if total else 0.0)
        self.avg_f.append(sum(self.f(c) for c in CPUS) / len(CPUS))
        self.temps = temps()
        self.on_sample()

    def on_sample(self):
        for k, v in self.temps.items():
            self.temp_hist.setdefault(k, deque(maxlen=HIST)).append(v)

    def f(self, c):
        return self.freq[c][-1] if self.freq[c] else 0.0

    def u(self, c):
        return self.load[c][-1] if self.load[c] else 0.0

    def domain_temp(self, d):
        """Temperature for an L3 domain: Tccd<n> on Ryzen, else the hottest sensor we have."""
        t = self.temps
        if f"Tccd{d + 1}" in t:
            return t[f"Tccd{d + 1}"]
        return t.get("Tctl", t.get("Package id 0"))


# ── palette ───────────────────────────────────────────────────────────────────

def hexrgb(h):
    return tuple(int(h[i:i + 2], 16) for i in (1, 3, 5))


def mix(a, b, t):
    a, b = hexrgb(a), hexrgb(b)
    return "#%02x%02x%02x" % tuple(round(a[k] + (b[k] - a[k]) * t) for k in range(3))


def ramp(*stops):
    n = len(stops) - 1
    table = []
    for q in range(65):
        x = q / 64
        i = min(int(x * n), n - 1)
        table.append(mix(stops[i], stops[i + 1], x * n - i))

    def color(x):
        return table[round(max(0.0, min(1.0, x)) * 64)]
    return color


BG = "#0b0d13"
PANEL = "#0e1118"
BORDER = "#252b3b"
TRACK = "#171b26"
TRACK_C = "#11141c"  # compact view: full-height cells, so a quieter track
FG = "#c8d0f0"
TEXT = "#a9b1d6"
MID = "#7a82a8"
DIM = "#4c5373"
ACCENT = "#7aa2f7"
SELECT = "#151c2c"
heat = ramp("#9ece6a", "#e0c068", "#ff9e64", "#f7768e")    # load, temperature
cool = ramp("#3d59a1", "#7aa2f7", "#bb9af7", "#ff7ac6")    # clock speed


def shade(pal, k):
    """A darker copy of a palette (blended towards the panel background)."""
    table = [mix(PANEL, pal(q / 64), k) for q in range(65)]
    return lambda x: table[round(max(0.0, min(1.0, x)) * 64)]


cool_idle = shade(cool, 0.5)
cool_idle_c = shade(cool, 0.3)  # compact view: full-height cells need a quieter idle color


# ── drawing primitives ────────────────────────────────────────────────────────

EIGHTHS = " ▏▎▍▌▋▊▉█"
SPARK = "▁▂▃▄"  # lower-half only, to line up with the half-height bars


def meter(frac, width, pal, track=TRACK):
    """Half-height gradient bar: '▄' leaves a gap between stacked rows, so every
    thread reads as its own line."""
    n = round(max(0.0, min(1.0, frac)) * width)
    t = Text(no_wrap=True)
    den = max(width - 1, 1)
    for i in range(width):
        t.append("▄", pal(i / den) if i < n else track)
    return t


def dual_meter(fa, fb, width, pal_a, pal_b, track=TRACK):
    """Two bars in one row: '▀' foreground is the upper bar, background the lower."""
    na = round(max(0.0, min(1.0, fa)) * width)
    nb = round(max(0.0, min(1.0, fb)) * width)
    t = Text(no_wrap=True)
    den = max(width - 1, 1)
    for i in range(width):
        x = i / den
        t.append("▀", f"{pal_a(x) if i < na else track} on {pal_b(x) if i < nb else track}")
    return t


def sparkline(values, width, pal, span_n):
    """Load history squeezed into `width` cells; each cell averages `span_n / width` samples."""
    vals = list(values)[-span_n:]
    k = max(1, len(vals) // width) if len(vals) >= width else 1
    buckets = [sum(vals[i:i + k]) / k for i in range(0, len(vals) - k + 1, k)][-width:]
    t = Text(no_wrap=True)
    t.append("▁" * (width - len(buckets)), TRACK)
    for v in buckets:
        if v < 0.05:  # ignore background noise
            t.append("▁", TRACK)
        else:
            t.append(SPARK[min(3, int(v * 4))], pal(v))
    return t


# braille dot bits per dot-row, bottom → top: (left column, right column)
BRAILLE = ((0x40, 0x80), (0x04, 0x20), (0x02, 0x10), (0x01, 0x08))


def graph_rows(values, width, height, vmax, pal, grid=(0.25, 0.5, 0.75)):
    """Braille area graph of the most recent 2*width values, colored by height,
    with faint dotted gridlines in the empty area."""
    width, height = max(1, width), max(1, height)
    vals = list(values)[-2 * width:]
    vals = [0.0] * (2 * width - len(vals)) + vals
    top = height * 4
    dots = [min(top, max(1 if v > 0 else 0, round(v / vmax * top))) for v in vals]
    gl = {round(g * top) for g in grid}
    rows = []
    for r in range(height):
        base = (height - 1 - r) * 4
        col = pal((height - r - 0.5) / height)
        grid_bit = next((k for k in range(4) if base + k + 1 in gl), None)
        t = Text(no_wrap=True)
        run, run_style = [], None
        for i in range(width):
            code = 0
            for k, (lb, rb) in enumerate(BRAILLE):
                if dots[2 * i] > base + k:
                    code |= lb
                if dots[2 * i + 1] > base + k:
                    code |= rb
            if code:
                ch, style = chr(0x2800 + code), col
            elif grid_bit is not None and i % 2 == 0:
                ch, style = chr(0x2800 + BRAILLE[grid_bit][0]), BORDER
            else:
                ch, style = " ", col
            if style != run_style and run:
                t.append("".join(run), run_style)
                run = []
            run_style = style
            run.append(ch)
        if run:
            t.append("".join(run), run_style)
        rows.append(t)
    return rows


def axis_graph(values, width, height, vmax, pal, labels):
    """Graph with a y-axis label gutter. `labels` maps fraction → text (e.g. {1: '100%'})."""
    gut = max(len(s) for s in labels.values()) + 1
    if width < gut + 8:
        return graph_rows(values, width, height, vmax, pal)
    rows = graph_rows(values, width - gut, height, vmax, pal)
    out = []
    marks = {}
    for frac, s in labels.items():
        marks.setdefault(min(height - 1, round((1 - frac) * (height - 1))), s)
    for r, line in enumerate(rows):
        t = Text(no_wrap=True)
        t.append(f"{marks.get(r, ''):>{gut - 1}} ", DIM)
        t.append_text(line)
        out.append(t)
    return out


def join_lines(lines):
    out = Text(no_wrap=True, overflow="crop")
    for i, l in enumerate(lines):
        if i:
            out.append("\n")
        out.append_text(l)
    return out


def ghz(f):
    return f"{f / 1000:4.2f}"


# ── widgets ───────────────────────────────────────────────────────────────────

SHORT_MODEL = " ".join(w for w in MODEL.replace("(R)", "").replace("(TM)", "").split()
                      if w not in ("Processor", "CPU") and not w.endswith("-Core"))


class TitleBar(Widget):
    def render(self):
        a = self.app
        brand = (" ◆ cpuview ", f"bold {BG} on {ACCENT}")
        topo = (f"   {len(a.s.groups)} cores · {len(CPUS)} threads", MID)
        driver = (f" · {DRIVER}", DIM) if DRIVER else ("", "")
        state = ((" PAUSED ", f"bold {BG} on #f7768e") if a.paused
                 else ("● ", "#9ece6a"))
        live = ("live " if not a.paused else "", MID)
        ival = (f" {INTERVALS[a.ival]:g}s", FG)
        up = [("   up ", DIM), (uptime(), TEXT)]
        clock = [("   ", ""), (time.strftime("%H:%M:%S") + " ", f"bold {FG}")]
        # most detailed first; drop the least useful parts until it fits
        for left, right in (
                ([brand, (f"  {MODEL}", f"bold {FG}"), topo, driver], [state, live, ival, *up, *clock]),
                ([brand, (f"  {SHORT_MODEL}", f"bold {FG}"), topo], [state, live, ival, *up, *clock]),
                ([brand, (f"  {SHORT_MODEL}", f"bold {FG}")], [state, live, ival, *up, *clock]),
                ([brand, (f"  {SHORT_MODEL}", f"bold {FG}")], [state, ival, *clock]),
                ([brand], [state, ival, *clock])):
            lt, rt = Text.assemble(*left), Text.assemble(*right)
            if lt.cell_len + rt.cell_len + 2 <= self.size.width:
                break
        gap = max(1, self.size.width - lt.cell_len - rt.cell_len)
        return Text.assemble(lt, " " * gap, rt, no_wrap=True, overflow="crop")


class HistGraph(Widget):
    def __init__(self, series, vmax, pal, labels, **kw):
        super().__init__(**kw)
        self.series, self.vmax, self.pal, self.labels = series, vmax, pal, labels

    def render(self):
        return join_lines(axis_graph(self.series(), self.size.width, self.size.height,
                                     self.vmax, self.pal, self.labels))


class Sensors(Widget):
    def render(self):
        a, s = self.app, self.app.s
        w = self.size.width
        lines = []
        bw = max(4, w - 6 - 7)
        for name, val in s.temps.items():
            x = (val - 30) / 65
            t = Text(no_wrap=True)
            t.append(f"{name:<6}", MID)
            t.append_text(meter(x, bw, heat))
            t.append(f" {val:4.0f}°C", f"bold {heat(x)}")
            lines.append(t)
        if not s.temps:
            lines.append(Text("no CPU temperature sensor found", DIM))
        lines.append(Text(""))
        gov = read(f"{SYS}/cpu0/cpufreq/scaling_governor", "?")
        epp = read(f"{SYS}/cpu0/cpufreq/energy_performance_preference")
        la = read("/proc/loadavg", "0 0 0").split()[:3]
        peak = max(s.peak.values())
        boosting = sum(1 for c in CPUS if s.f(c) >= BOOST)
        busy = sum(1 for c in CPUS if s.u(c) >= 0.5)
        lines += [
            Text.assemble(("policy  ", DIM), (gov, TEXT), (f" · {epp}" if epp else "", MID)),
            Text.assemble(("loadavg ", DIM), ("  ".join(la), TEXT)),
            Text.assemble(("active  ", DIM), (f"{busy}", FG), (f"/{len(CPUS)} busy", MID),
                          ("  ", ""), (f"{boosting}", cool(1.0) if boosting else FG),
                          (" boosting", MID)),
            Text.assemble(("peak    ", DIM), (ghz(peak), f"bold {cool(peak / MAXF)}"),
                          (" GHz", MID), (f"  of {ghz(MAXF)}", DIM)),
        ]
        h = self.size.height
        if len(lines) > h and "" == lines[len(s.temps) or 1].plain:
            del lines[len(s.temps) or 1]
        name = next(iter(s.temps), None)
        gh = h - len(lines) - 2
        if name and gh >= 3:  # spare height: temperature history
            win = span(2 * (w - 5) * INTERVALS[a.ival])
            lines += [Text(""), Text.assemble((f"{name} history", MID), (f" · last {win}", DIM))]
            hist = [max(0.0, v - 30) for v in s.temp_hist.get(name, ())]
            lines += axis_graph(hist, w, gh, 65, heat, {1: "95°", 0.5: "63°", 0: "30°"})
        return join_lines(lines[:h])


def card_layout(width, compact):
    """Cell widths of a core card's fields: (freq bar, load bar, sparkline)."""
    if compact:
        fixed = 1 + 3 + 1 + 1 + 9 + 2 + 1 + 7
        avail = max(8, width - fixed)
        fw = max(4, round(avail * 0.6))
        return fw, max(3, avail - fw), 0
    fixed = 1 + 3 + 1 + 3 + 1 + 1 + 4 + 2 + 1 + 4
    sw = 10 if width >= 74 else (6 if width >= 64 else 0)
    avail = max(8, width - fixed - (sw + 2 if sw else 0))
    fw = max(4, round(avail * 0.6))
    return fw, max(3, avail - fw), sw


class CoreCard(Widget):
    def __init__(self, slot):
        super().__init__()
        self.slot = slot

    def group(self):
        return self.app.slot_group(self.slot)

    def render(self):
        s = self.app.s
        gi = self.group()
        if gi is None:
            return Text("")
        if self.app.compact and len(s.groups[gi]) >= 2:
            return self.render_compact(gi)
        a = self.app
        sel = gi == a.selected
        fw, lw, sw = card_layout(self.size.width, False)
        span_n = max(1, round(SPARK_SECONDS / INTERVALS[a.ival]))
        threads = s.groups[gi]
        active = any(s.u(c) >= 0.05 for c in threads)
        t = Text(no_wrap=True, overflow="crop")
        for j, c in enumerate(threads):
            f, u = s.f(c), s.u(c)
            idle = u < 0.01 and f <= MINF * 1.05 + 1
            t.append("▌" if sel else " ", ACCENT)
            if j == 0:
                t.append(f"C{gi:02}", f"bold {FG if sel or active else MID}")
            else:
                t.append("   ")
            t.append(f" t{c:02} ", DIM)
            t.append_text(meter(f / MAXF, fw, cool_idle if idle else cool))
            t.append(" ")
            if f >= BOOST:
                t.append(ghz(f), f"bold {cool(f / MAXF)}")
            else:
                t.append(ghz(f), DIM if idle else cool(max(0.45, f / MAXF)))
            t.append("  ")
            t.append_text(meter(u, lw, heat))
            t.append(" ")
            t.append(f"{u * 100:3.0f}", DIM if u < 0.01 else f"bold {heat(u)}" if u >= 0.9 else heat(u))
            t.append("%", DIM)
            if sw:
                t.append("  ")
                t.append_text(sparkline(s.load[c], sw, heat, span_n))
            if j < len(threads) - 1:
                t.append("\n")
        return t

    def render_compact(self, gi):
        a, s = self.app, self.app.s
        sel = gi == a.selected
        ta, tb = s.groups[gi][:2]
        fa, fb, ua, ub = s.f(ta), s.f(tb), s.u(ta), s.u(tb)
        fw, lw, _ = card_layout(self.size.width, True)
        active = max(ua, ub) >= 0.05
        t = Text(no_wrap=True, overflow="crop")
        t.append("▌" if sel else " ", ACCENT)
        t.append(f"C{gi:02} ", f"bold {FG if sel or active else MID}")
        idle = [u < 0.01 and f <= MINF * 1.05 + 1 for f, u in ((fa, ua), (fb, ub))]
        t.append_text(dual_meter(fa / MAXF, fb / MAXF, fw, *(cool_idle_c if i else cool for i in idle),
                                 track=TRACK_C))
        for (f, u), idl in zip(((fa, ua), (fb, ub)), idle):
            st = f"bold {cool(f / MAXF)}" if f >= BOOST else DIM if idl else cool(max(0.45, f / MAXF))
            t.append(" ")
            t.append(ghz(f), st)
        t.append("  ")
        t.append_text(dual_meter(ua, ub, lw, heat, heat, track=TRACK_C))
        for u in (ua, ub):
            t.append(f" {u * 100:3.0f}", DIM if u < 0.01 else heat(u))
        return t

    def on_click(self, event: events.Click):
        gi = self.group()
        if gi is not None:
            self.app.select(gi)


class CoresHeader(Widget):
    """Column legend above the core grid; shows the CCD and its temperature per column."""

    def render(self):
        a, s = self.app, self.app.s
        cards = list(a.query(CoreCard))[:a.cols]
        x0 = self.region.x
        out = Text(no_wrap=True, overflow="crop")
        for col, card in enumerate(cards):
            pad = card.region.x - x0 - out.cell_len
            if pad > 0:
                out.append(" " * pad)
            members = [g for g in (a.slot_group(i) for i in range(col, len(a.slots), a.cols))
                       if g is not None]
            doms = {s.ccd[g] for g in members}
            ndom = len(set(s.ccd))
            fw, lw, sw = card_layout(card.region.width, a.compact)
            t = Text(no_wrap=True)

            def at(col, text):  # place dim legend text starting at a fixed column
                t.append(" " * max(1, col - t.cell_len))
                t.append(text, DIM)

            if ndom > 1 and len(doms) == 1 and SORTS[a.sort] == "core":
                d = doms.pop()
                temp = s.domain_temp(d)
                t.append(f" CCD{d}", f"bold {TEXT}")
                if temp is not None:
                    t.append(f" {temp:.0f}°", heat((temp - 30) / 65))
            else:
                t.append(" cpu" if a.compact else " core thr", DIM)
            if a.compact:  # " C03 " [freq bar] " 5.53 5.50" "  " [load bar] "  97 100"
                at(5 if t.cell_len < 5 else t.cell_len + 1, "clock")
                at(5 + fw + 10 - 3, "GHz")
                at(5 + fw + 12, "load")
                at(5 + fw + 12 + lw + 8 - 1, "%")
            else:  # " C03 t03 " [freq bar] " 5.53" "  " [load bar] " 97%" "  " [trend]
                at(9, "clock")
                at(9 + fw + 5 - 3, "GHz")
                at(9 + fw + 7, "load")
                at(9 + fw + 7 + lw + 4, "%")
                if sw:
                    at(9 + fw + 7 + lw + 7, f"{SPARK_SECONDS}s trend" if sw >= 9 else f"{SPARK_SECONDS}s")
            t.truncate(card.region.width)
            out.append_text(t)
        return out


class CoresPanel(VerticalScroll):
    pass


class Detail(Widget):
    def render(self):
        a, s = self.app, self.app.s
        gi = a.selected
        threads = s.groups[gi]
        w, h = self.size.width, self.size.height
        stats_w = 30 if w >= 110 else 0
        gap, n = 3, len(threads)
        cw = max(12, (w - stats_w - gap * n) // n) if stats_w else max(12, (w - gap * (n - 1)) // n)
        lh = max(1, (h - 1) // 3)
        fh = max(1, h - 1 - lh)
        cols = []
        for c in threads:
            f, u = s.f(c), s.u(c)
            head = Text.assemble((f"t{c:02}", f"bold {FG}"), ("  ", ""),
                                 (ghz(f), f"bold {cool(f / MAXF)}"), (" GHz", MID),
                                 ("   load ", DIM), (f"{u * 100:.0f}%", f"bold {heat(u)}"),
                                 ("   peak ", DIM), (ghz(s.peak[c]), TEXT))
            lines = ([head]
                     + axis_graph(s.freq[c], cw, fh, MAXF, cool,
                                  {1: ghz(MAXF), 0: "GHz"} if fh >= 2 else {1: ghz(MAXF)})
                     + axis_graph(s.load[c], cw, lh, 1.0, heat,
                                  {1: "100%", 0: "load"} if lh >= 2 else {0.5: "load"}))
            cols.append(lines)
        if stats_w:
            cols.append(self.stats(gi, h, 2 * (cw - 5)))
        out = Text(no_wrap=True, overflow="crop")
        for r in range(h):
            for i, lines in enumerate(cols):
                line = lines[r].copy() if r < len(lines) else Text("")
                last = i == len(cols) - 1
                width = stats_w if (stats_w and last) else cw
                line.truncate(width, pad=not last)
                if i:
                    out.append(" │ " if not (stats_w and last) else " ┃ ", BORDER)
                out.append_text(line)
            if r < h - 1:
                out.append("\n")
        return out

    def stats(self, gi, h, win):
        a, s = self.app, self.app.s
        threads = s.groups[gi]
        fs = [f for c in threads for f in list(s.freq[c])[-win:]]
        us = [u for c in threads for u in list(s.load[c])[-win:]]
        avg_f = sum(fs) / len(fs) if fs else 0.0
        avg_u = sum(us) / len(us) if us else 0.0
        boost = sum(1 for f in fs if f >= BOOST) / len(fs) if fs else 0.0
        peak = max(s.peak[c] for c in threads)
        rank = s.rank[gi]
        lines = [Text.assemble((f"C{gi:02}", f"bold {ACCENT}"),
                               (f"  ·  CCD{s.ccd[gi]}" if len(set(s.ccd)) > 1 else "", MID),
                               (f"  ·  {len(threads)} threads", MID))]
        if rank >= 0:
            pos = 1 + sum(1 for r in s.rank if r > rank)
            lines.append(Text.assemble(("pref.   ", DIM), (f"#{pos}", f"bold {FG}"),
                                       (f" of {len(s.rank)}", MID),
                                       ("  ★ best" if pos == 1 else "", "#e0af68")))
        lines += [
            Text.assemble(("avg     ", DIM), (ghz(avg_f), cool(avg_f / MAXF)), (" GHz", MID)),
            Text.assemble(("peak    ", DIM), (ghz(peak), cool(peak / MAXF)), (" GHz", MID)),
            Text.assemble(("boost   ", DIM), (f"{boost * 100:.0f}%", FG), (" of the time", MID)),
            Text.assemble(("load    ", DIM), (f"{avg_u * 100:.0f}%", heat(avg_u)), (" average", MID)),
            Text.assemble(("window  ", DIM), (span(win * INTERVALS[a.ival]), TEXT)),
        ]
        return lines[:h]


class KeyBar(Widget):
    """Footer: key hints with the current setting of each toggle (click to press)."""
    KEYS = (("q", "quit", "quit"), ("space", "pause", "pause"), ("s", "sort", "sort"),
            ("c", "view", "view"), ("+-", "interval", ""), ("g", "graphs", "graphs"),
            ("d", "detail", "detail"), ("r", "reset peaks", "peaks"), ("←↑↓→", "select", None))
    LABEL = {"space": "spc", "+-": "+ -"}

    def state(self, key):
        a = self.app
        return {"s": SORTS[a.sort], "c": VIEWS[a.view], "+-": f"{INTERVALS[a.ival]:g}s"}.get(key)

    def build(self, short, pad, sep):
        t = Text(no_wrap=True, overflow="crop")
        zones = []
        for key, desc, sdesc in self.KEYS:
            d = "" if short is None else sdesc if short else desc
            if d is None or (short is None and sdesc is None):
                continue
            x = t.cell_len
            k = self.LABEL.get(key, key)
            t.append(f" {k} " if pad else k, f"bold {ACCENT} on #1a2133")
            if d:
                t.append(f" {d}", MID)
            st = self.state(key)
            if st:
                t.append(f" {st}", FG)
            t.append(sep)
            zones.append((x, t.cell_len, key))
        return t, zones

    def render(self):
        w = self.size.width
        for args in ((False, True, "  "), (True, True, "  "), (True, True, " "), (True, False, " "),
                     (None, False, " ")):
            t, self.zones = self.build(*args)
            t.rstrip()
            if t.cell_len <= w:
                break
        return t

    def on_click(self, event: events.Click):
        for x0, x1, key in getattr(self, "zones", ()):
            if x0 <= event.x < x1 and key not in ("←↑↓→", "+-"):
                self.app.simulate_key(key)


# ── app ───────────────────────────────────────────────────────────────────────

class CpuView(App):
    ENABLE_COMMAND_PALETTE = False
    CSS = f"""
    Screen {{ background: {BG}; color: {TEXT}; }}
    #title {{ height: 1; background: #121621; }}
    #keys {{ height: 1; background: #121621; dock: bottom; }}
    #top {{ height: 10; }}
    .panel {{
        border: round {BORDER};
        border-title-color: {FG}; border-title-style: bold;
        border-subtitle-color: {DIM};
        padding: 0 1;
        background: {PANEL};
    }}
    #g_load, #g_freq {{ width: 1fr; }}
    #sensors {{ width: 46; }}
    #cores_wrap {{ height: 1fr; scrollbar-size-vertical: 1; }}
    #cores_head {{ height: 1; }}
    #cores {{ grid-size: 2; grid-gutter: 0 3; grid-rows: 2; height: auto; }}
    #cores.compact {{ grid-rows: 1; }}
    CoreCard {{ height: 2; }}
    #cores.compact CoreCard {{ height: 1; }}
    CoreCard.selected {{ background: {SELECT}; }}
    #detail {{ height: 9; }}
    """

    BINDINGS = [
        Binding("q", "quit", "Quit"),
        Binding("space", "pause", "Pause"),
        Binding("s", "cycle_sort", "Sort"),
        Binding("c", "cycle_view", "View"),
        Binding("plus,equals_sign", "interval(1)", "Slower", key_display="+"),
        Binding("minus", "interval(-1)", "Faster", key_display="-"),
        Binding("g", "toggle_panel('#top')", "Graphs"),
        Binding("d,enter", "toggle_panel('#detail')", "Detail"),
        Binding("r", "reset_peaks", "Reset peaks"),
        Binding("up,k", "move(0, -1)", show=False, priority=True),
        Binding("down,j", "move(0, 1)", show=False, priority=True),
        Binding("left,h", "move(-1, 0)", show=False, priority=True),
        Binding("right,l", "move(1, 0)", show=False, priority=True),
    ]

    def __init__(self):
        super().__init__()
        self.s = Sampler()
        n = len(self.s.groups)
        self.order = list(range(n))
        self.selected = 0
        self.sort = 0
        self.ival = INTERVALS.index(1.0)
        self.paused = False
        self.cols = 2
        self.nrows = -(-n // 2)
        self.slots = list(range(n))
        self.view = 0
        self.compact = False
        self.sized = False

    def compose(self) -> ComposeResult:
        yield TitleBar(id="title")
        with Horizontal(id="top"):
            yield HistGraph(lambda: self.s.tot_u, 1.0, heat,
                            {1: "100%", 0.5: "50", 0: "0"}, id="g_load", classes="panel")
            yield HistGraph(lambda: self.s.avg_f, MAXF, cool,
                            {1: ghz(MAXF), 0.5: ghz(MAXF / 2), 0: "0"}, id="g_freq", classes="panel")
            yield Sensors(id="sensors", classes="panel")
        with CoresPanel(id="cores_wrap", classes="panel"):
            yield CoresHeader(id="cores_head")
            with Grid(id="cores"):
                for i in range(len(self.s.groups) + 3):
                    yield CoreCard(i)
        yield Detail(id="detail", classes="panel")
        yield KeyBar(id="keys")

    def on_mount(self):
        self.query_one("#sensors").border_title = "sensors"
        self.timer = self.set_interval(INTERVALS[self.ival], self.tick)
        self.set_timer(0.2, self.tick)
        self.refresh_views()

    # layout ------------------------------------------------------------------

    def slot_group(self, slot):
        k = self.slots[slot] if slot < len(self.slots) else None
        return self.order[k] if k is not None else None

    def graph_width(self):
        return max(1, self.query_one("#g_load").size.width - 5)

    def layout_cores(self, w, h):
        """Best (compact, cols, nrows) for an inner area of w×h cells, or None if nothing fits."""
        n = len(self.s.groups)
        mode = VIEWS[self.view]
        for compact, minw in ((False, 52), (True, 40)):
            if mode != "auto" and (mode == "compact") != compact:
                continue
            maxc = max(1, min(n, (w + 3) // (minw + 3)))
            # don't stretch cards across very wide terminals
            least = min(maxc, max(1, -(-(w + 3) // ((90 if compact else 120) + 3))))
            rh = 1 if compact else 2
            fitting = [c for c in range(least, maxc + 1) if -(-n // c) * rh <= h]
            # prefer column counts that split the cores evenly (keeps CCDs in their own columns)
            cols = next((c for c in fitting if n % c == 0), fitting[0] if fitting else None)
            if cols:
                return compact, cols, -(-n // cols)
        return None

    def fit(self):
        """Size the panels and pick the core-grid view/columns that show every core at once."""
        W, H = self.size
        if W <= 0 or H <= 0:
            return
        top, det, wrap = self.query_one("#top"), self.query_one("#detail"), self.query_one("#cores_wrap")
        avail = H - 2  # title bar + key bar
        n = len(self.s.groups)
        # graphs and detail shrink before the cores give up their no-scroll guarantee
        for top_min, det_min in ((10, 8), (7, 6), (5, 5)):
            room = avail - (top_min if top.display else 0) - (det_min if det.display else 0)
            h = room - 2  # inside the cores panel border
            choice = self.layout_cores(W - 4, h)
            if choice:
                break
        if choice:
            compact, cols, nrows = choice
            rh = 1 if compact else 2
            need = nrows * rh
            header = need + 1 <= h
            gap = header and need + 1 + (nrows - 1) <= h and not (top.display and det.display)
            used = need + header + (nrows - 1 if gap else 0) + 2
        else:  # nothing fits: densest layout, scroll the rest
            compact = VIEWS[self.view] != "full"
            cols = max(1, min(n, (W - 1) // (43 if compact else 55)))
            nrows = -(-n // cols)
            header, gap = True, False
        self.compact, self.cols, self.nrows = compact, cols, nrows
        self.slots = [c * nrows + r if c * nrows + r < n else None
                      for r in range(nrows) for c in range(cols)]
        grid = self.query_one("#cores")
        grid.set_class(compact, "compact")
        grid.styles.grid_size_columns = cols
        grid.styles.grid_gutter_horizontal = 1 if gap else 0  # (Textual: "horizontal" = between rows)
        self.query_one("#cores_head").display = header
        for card in self.query(CoreCard):
            card.display = card.slot < len(self.slots)
        # panel heights: cores get exactly what they need, graphs and detail share the rest
        if choice and (top.display or det.display):
            wrap.styles.height = used
            rest = avail - used
            if top.display and det.display:
                th = min(rest - det_min, max(top_min, round(rest * 0.55)))
                top.styles.height, det.styles.height = th, rest - th
            elif top.display:
                top.styles.height = rest
            else:
                det.styles.height = rest
        else:
            wrap.styles.height = "1fr"
            top.styles.height, det.styles.height = top_min, det_min
        self.refresh_views()

    def on_resize(self, event: events.Resize):
        if not self.sized:  # start compact on short terminals; g / d bring panels back
            self.sized = True
            self.query_one("#top").display = event.size.height >= 32
            self.query_one("#detail").display = event.size.height >= 40
        self.call_after_refresh(self.fit)

    # updates -----------------------------------------------------------------

    def tick(self):
        self.s.sample()
        try:
            self.refresh_views()
        except NoMatches:  # a tick that lands while the app is shutting down
            pass

    def refresh_views(self):
        s = self.s
        key = SORTS[self.sort]
        ids = range(len(s.groups))
        if key == "load":
            self.order = sorted(ids, key=lambda g: -max(s.u(c) for c in s.groups[g]))
        elif key == "freq":
            self.order = sorted(ids, key=lambda g: -max(s.f(c) for c in s.groups[g]))
        else:
            self.order = list(ids)

        u = s.tot_u[-1] if s.tot_u else 0.0
        avg = s.avg_f[-1] if s.avg_f else 0.0
        top = max((s.f(c) for c in CPUS), default=0.0)
        window = span(2 * self.graph_width() * INTERVALS[self.ival])
        g = self.query_one("#g_load")
        g.border_title = Text.assemble(("cpu ", f"bold {FG}"), (f"{u * 100:.0f}%", f"bold {heat(u)}"))
        g.border_subtitle = f"total load · last {window}"
        g = self.query_one("#g_freq")
        g.border_title = Text.assemble(("clock ", f"bold {FG}"), ("avg ", MID),
                                       (ghz(avg), f"bold {cool(avg / MAXF)}"),
                                       ("  top ", MID), (ghz(top), f"bold {cool(top / MAXF)}"),
                                       (" GHz", MID))
        g.border_subtitle = f"average clock · last {window}"
        wrap = self.query_one("#cores_wrap")
        view = "compact" if self.compact else "full"
        if VIEWS[self.view] == "auto":
            view = f"auto · {view}"
        wrap.border_title = Text.assemble(("cores ", f"bold {FG}"),
                                          (f"{len(s.groups)}", TEXT), ("   sort ", MID), (key, TEXT),
                                          ("   view ", MID), (view, TEXT))
        wrap.border_subtitle = ("▀ upper = first thread  ▄ lower = sibling" if self.compact
                                else "↑↓←→ / click to select")
        threads = s.groups[self.selected]
        d = self.query_one("#detail")
        d.border_title = Text.assemble((f"C{self.selected:02} ", f"bold {FG}"),
                                       (" + ".join(f"t{c:02}" for c in threads), MID))
        d.border_subtitle = "clock (top) · load (bottom)"

        for card in self.query(CoreCard):
            card.set_class(card.group() == self.selected and card.display, "selected")
            card.refresh()
        for w in ("#title", "#g_load", "#g_freq", "#sensors", "#detail", "#cores_head", "#keys"):
            self.query_one(w).refresh()

    def select(self, gi):
        self.selected = gi
        self.refresh_views()

    def action_move(self, dx, dy):
        k = self.order.index(self.selected)
        c, r = divmod(k, self.nrows)
        c, r = c + dx, r + dy
        j = c * self.nrows + r
        if 0 <= c < self.cols and 0 <= r < self.nrows and j < len(self.order):
            self.select(self.order[j])
            slot = self.slots.index(j)
            self.query(CoreCard)[slot].scroll_visible()

    def action_cycle_sort(self):
        self.sort = (self.sort + 1) % len(SORTS)
        self.refresh_views()

    def action_cycle_view(self):
        self.view = (self.view + 1) % len(VIEWS)
        self.fit()

    def action_interval(self, d):
        self.ival = max(0, min(len(INTERVALS) - 1, self.ival + d))
        self.timer.stop()
        self.timer = self.set_interval(INTERVALS[self.ival], self.tick, pause=self.paused)
        self.refresh_views()

    def action_pause(self):
        self.paused = not self.paused
        (self.timer.pause if self.paused else self.timer.resume)()
        self.refresh_views()

    def action_toggle_panel(self, sel):
        w = self.query_one(sel)
        w.display = not w.display
        self.fit()

    def action_reset_peaks(self):
        for c in CPUS:
            self.s.peak[c] = self.s.f(c)
        self.refresh_views()


if __name__ == "__main__":
    CpuView().run()

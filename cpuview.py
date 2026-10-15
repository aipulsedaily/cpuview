#!/usr/bin/env python3
"""cpuview — btop-style interactive per-core frequency / load / temperature monitor.

Keys:  arrows / hjkl  select core      enter / d  toggle detail panel
       s  cycle sort (core/load/freq)  g  toggle history graphs
       c  cycle view (auto/full/compact — auto fits every core on screen)
       + / -  update interval          space  pause      r  reset peaks
       q  quit                         mouse: click a core to select it
"""
import glob
import os
import time
from collections import deque

from rich.console import Group
from rich.table import Table
from rich.text import Text
from textual import events
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Grid, Horizontal, VerticalScroll
from textual.widget import Widget
from textual.widgets import Footer

SYS = "/sys/devices/system/cpu"
HIST = 1000
INTERVALS = (0.25, 0.5, 1.0, 2.0, 5.0)
SORTS = ("core", "load", "freq")
VIEWS = ("auto", "full", "compact")


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


CPUS = parse_list(read(f"{SYS}/online", "0"))
MAXF = max(int(read(f"{SYS}/cpu{c}/cpufreq/cpuinfo_max_freq", 0) or 0) for c in CPUS) / 1000 or 1
MODEL = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo")
              if l.startswith("model name")), "CPU")


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


class Sampler:
    def __init__(self):
        self.groups = core_groups()
        self.freq = {c: deque(maxlen=HIST) for c in CPUS}
        self.load = {c: deque(maxlen=HIST) for c in CPUS}
        self.avg_f = deque(maxlen=HIST)
        self.tot_u = deque(maxlen=HIST)
        self.peak = dict.fromkeys(CPUS, 0.0)
        self.temps = {}
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

    def f(self, c):
        return self.freq[c][-1] if self.freq[c] else 0.0

    def u(self, c):
        return self.load[c][-1] if self.load[c] else 0.0


# ── drawing ───────────────────────────────────────────────────────────────────

def ramp(*stops):
    def color(x):
        x = max(0.0, min(1.0, x))
        n = len(stops) - 1
        i = min(int(x * n), n - 1)
        t = x * n - i
        a, b = stops[i], stops[i + 1]
        return "#%02x%02x%02x" % tuple(round(a[k] + (b[k] - a[k]) * t) for k in range(3))
    return color


heat = ramp((70, 200, 100), (230, 200, 60), (240, 70, 60))     # load, temperature
cool = ramp((70, 130, 240), (160, 100, 240), (240, 90, 170))   # frequency
EMPTY = "#262a33"
DIM = "#5d6470"
MID = "#8a8f98"
ACCENT = "#8fb4ff"


def bar(frac, width, pal):
    frac = max(0.0, min(1.0, frac))
    n = round(frac * width)
    t = Text(no_wrap=True)
    for i in range(width):
        t.append("■", style=pal(i / max(width - 1, 1)) if i < n else EMPTY)
    return t


def dual_bar(fa, fb, width, pal):
    """Two bars in one row: '▀' foreground is the upper bar, background the lower."""
    na = round(max(0.0, min(1.0, fa)) * width)
    nb = round(max(0.0, min(1.0, fb)) * width)
    t = Text(no_wrap=True)
    for i in range(width):
        col = pal(i / max(width - 1, 1))
        t.append("▀", style=f"{col if i < na else EMPTY} on {col if i < nb else EMPTY}")
    return t


# braille dot bits per dot-row, bottom → top: (left column, right column)
BRAILLE = ((0x40, 0x80), (0x04, 0x20), (0x02, 0x10), (0x01, 0x08))


def graph(values, width, height, vmax, pal):
    """Braille area graph of the most recent 2*width values, colored by height."""
    width, height = max(1, width), max(1, height)
    vals = list(values)[-2 * width:]
    vals = [0.0] * (2 * width - len(vals)) + vals
    top = height * 4
    dots = [min(top, max(1 if v > 0 else 0, round(v / vmax * top))) for v in vals]
    t = Text(no_wrap=True)
    for r in range(height):
        base = (height - 1 - r) * 4
        row = []
        for i in range(width):
            code = 0x2800
            for k, (lb, rb) in enumerate(BRAILLE):
                if dots[2 * i] > base + k:
                    code |= lb
                if dots[2 * i + 1] > base + k:
                    code |= rb
            row.append(chr(code))
        t.append("".join(row), style=pal((height - r - 0.5) / height))
        if r < height - 1:
            t.append("\n")
    return t


# ── widgets ───────────────────────────────────────────────────────────────────

class TitleBar(Widget):
    def render(self):
        a = self.app
        left = Text.assemble((f" {MODEL}", "bold #e6e6e6"),
                             (f"  {len(a.s.groups)}C/{len(CPUS)}T", MID))
        right = Text.assemble(("PAUSED  " if a.paused else "", "bold #f05050"),
                              (f"up {uptime()}", MID), ("  ·  ", DIM),
                              (f"{INTERVALS[a.ival]:g}s", MID), ("  ·  ", DIM),
                              (time.strftime("%H:%M:%S") + " ", "#e6e6e6"))
        gap = max(1, self.size.width - left.cell_len - right.cell_len)
        return Text.assemble(left, " " * gap, right, no_wrap=True)


class HistGraph(Widget):
    def __init__(self, series, vmax, pal, **kw):
        super().__init__(**kw)
        self.series, self.vmax, self.pal = series, vmax, pal

    def render(self):
        return graph(self.series(), self.size.width, self.size.height, self.vmax, self.pal)


class Sensors(Widget):
    def render(self):
        s = self.app.s
        t = Table.grid(padding=(0, 1))
        for _ in range(3):
            t.add_column(no_wrap=True)
        bw = max(4, self.size.width - 6 - 5 - 2)
        for name, val in s.temps.items():
            x = (val - 30) / 65
            t.add_row(Text(f"{name:<5}", style=MID), bar(x, bw, heat),
                      Text(f"{val:3.0f}°C", style=heat(x)))
        if not s.temps:
            t.add_row(Text("no CPU temperature sensor found", style=DIM), "", "")
        gov = read(f"{SYS}/cpu0/cpufreq/scaling_governor", "?")
        epp = read(f"{SYS}/cpu0/cpufreq/energy_performance_preference")
        la = read("/proc/loadavg", "0 0 0").split()[:3]
        peak = max(s.peak.values())
        info = Text.assemble(
            ("gov   ", DIM), (gov, MID), (f" · {epp}" if epp else "", MID), "\n",
            ("load  ", DIM), (" ".join(la), MID), "\n",
            ("peak  ", DIM), (f"{peak:.0f} MHz", cool(peak / MAXF)),
            (f"  of {MAXF:.0f}", DIM), no_wrap=True)
        return Group(t, Text(""), info)


class CoreCard(Widget):
    def __init__(self, slot):
        super().__init__()
        self.slot = slot

    def group(self):
        order = self.app.order
        return order[self.slot] if self.slot < len(order) else None

    def render(self):
        s = self.app.s
        gi = self.group()
        if gi is None:
            return Text("")
        if self.app.compact and len(s.groups[gi]) >= 2:
            return self.render_compact(gi)
        avail = max(8, self.size.width - 23)
        fw = max(4, round(avail * 0.62))
        lw = max(3, avail - fw)
        t = Text(no_wrap=True, overflow="crop")
        for j, c in enumerate(s.groups[gi]):
            f, u = s.f(c), s.u(c)
            t.append(f"C{gi:02} " if j == 0 else "    ", style=f"bold {ACCENT}")
            t.append(f"t{c:02} ", style=DIM)
            t.append_text(bar(f / MAXF, fw, cool))
            t.append(f" {f:5.0f}MHz", style=cool(f / MAXF))
            t.append(" ")
            t.append_text(bar(u, lw, heat))
            t.append(f"{u * 100:4.0f}%", style=heat(u))
            if j < len(s.groups[gi]) - 1:
                t.append("\n")
        return t

    def render_compact(self, gi):
        s = self.app.s
        a, b = s.groups[gi][:2]
        fa, fb, ua, ub = s.f(a), s.f(b), s.u(a), s.u(b)
        avail = max(8, self.size.width - 23)
        fw = max(4, round(avail * 0.62))
        lw = max(3, avail - fw)
        t = Text(no_wrap=True, overflow="crop")
        t.append(f"C{gi:02} ", style=f"bold {ACCENT}")
        t.append_text(dual_bar(fa / MAXF, fb / MAXF, fw, cool))
        t.append(f" {fa:4.0f}", style=cool(fa / MAXF))
        t.append(f" {fb:4.0f}", style=cool(fb / MAXF))
        t.append(" ")
        t.append_text(dual_bar(ua, ub, lw, heat))
        t.append(f" {ua * 100:3.0f}", style=heat(ua))
        t.append(f"{ub * 100:4.0f}", style=heat(ub))
        return t

    def on_click(self, event: events.Click):
        gi = self.group()
        if gi is not None:
            self.app.select(gi)


class CoresPanel(VerticalScroll):
    def on_resize(self, event: events.Resize):
        self.app.fit()


class Detail(Widget):
    def render(self):
        a, s = self.app, self.app.s
        threads = s.groups[a.selected]
        w, h = self.size.width, self.size.height
        gap, n = 3, len(threads)
        cw = max(10, (w - gap * (n - 1)) // n)
        lh = max(1, (h - 1) // 3)
        fh = max(1, h - 1 - lh)
        cols = []
        for c in threads:
            f, u = s.f(c), s.u(c)
            head = Text.assemble((f"t{c:02}  ", "bold #e6e6e6"),
                                 (f"{f:.0f} MHz", cool(f / MAXF)),
                                 ("  peak ", DIM), (f"{s.peak[c]:.0f}", MID),
                                 ("  load ", DIM), (f"{u * 100:.0f}%", heat(u)))
            lines = ([head]
                     + list(graph(s.freq[c], cw, fh, MAXF, cool).split("\n"))
                     + list(graph(s.load[c], cw, lh, 1.0, heat).split("\n")))
            cols.append(lines)
        out = Text(no_wrap=True)
        for r in range(len(cols[0])):
            for i, lines in enumerate(cols):
                line = lines[r].copy()
                line.truncate(cw, pad=True)
                out.append_text(line)
                if i < n - 1:
                    out.append(" " * gap)
            if r < len(cols[0]) - 1:
                out.append("\n")
        return out


# ── app ───────────────────────────────────────────────────────────────────────

class CpuView(App):
    CSS = """
    Screen { background: #0d0f13; color: #c9ced6; }
    #title { height: 1; background: #161a21; }
    #top { height: 11; }
    .panel {
        border: round #353b47;
        border-title-color: #8fb4ff; border-title-style: bold;
        border-subtitle-color: #5d6470;
        padding: 0 1;
        background: #0d0f13;
    }
    #g_load, #g_freq { width: 1fr; }
    #sensors { width: 42; }
    #cores_wrap { height: 1fr; scrollbar-size-vertical: 1; }
    #cores { grid-size: 2; grid-gutter: 0 4; grid-rows: 2; height: auto; }
    #cores.compact { grid-rows: 1; }
    CoreCard { height: 2; }
    #cores.compact CoreCard { height: 1; }
    CoreCard.selected { background: #1b2433; }
    #detail { height: 9; }
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
        self.order = list(range(len(self.s.groups)))
        self.selected = 0
        self.sort = 0
        self.ival = INTERVALS.index(1.0)
        self.paused = False
        self.cols = 2
        self.view = 0
        self.compact = False
        self.sized = False

    def compose(self) -> ComposeResult:
        yield TitleBar(id="title")
        with Horizontal(id="top"):
            yield HistGraph(lambda: self.s.tot_u, 1.0, heat, id="g_load", classes="panel")
            yield HistGraph(lambda: self.s.avg_f, MAXF, cool, id="g_freq", classes="panel")
            yield Sensors(id="sensors", classes="panel")
        with CoresPanel(id="cores_wrap", classes="panel"):
            with Grid(id="cores"):
                for i in range(len(self.s.groups)):
                    yield CoreCard(i)
        yield Detail(id="detail", classes="panel")
        yield Footer()

    def on_mount(self):
        self.query_one("#sensors").border_title = "sensors"
        self.timer = self.set_interval(INTERVALS[self.ival], self.tick)
        self.set_timer(0.2, self.tick)
        self.refresh_views()

    def fit(self):
        """Pick the view and column count that shows every core without scrolling."""
        wrap = self.query_one("#cores_wrap")
        w, h = wrap.size.width - 1, wrap.size.height  # leave room for a scrollbar
        if w <= 0 or h <= 0:
            return
        n = len(self.s.groups)
        mode = VIEWS[self.view]
        options = [o for o in ((False, 56), (True, 40))
                   if mode == "auto" or (mode == "compact") == o[0]]
        for compact, minw in options:
            maxc = max(1, min(n, (w + 4) // (minw + 4)))
            least = min(maxc, max(1, w // 140))  # don't stretch bars across very wide terminals
            rows = 1 if compact else 2
            cols = next((c for c in range(least, maxc + 1) if -(-n // c) * rows <= h), None)
            if cols:
                break
        else:
            cols = maxc  # nothing fits: densest layout, scroll the rest
        self.compact, self.cols = compact, cols
        grid = self.query_one("#cores")
        grid.set_class(compact, "compact")
        grid.styles.grid_size_columns = cols
        self.refresh_views()

    def on_resize(self, event: events.Resize):
        if not self.sized:  # start compact on short terminals; g / d bring panels back
            self.sized = True
            self.query_one("#top").display = event.size.height >= 32
            self.query_one("#detail").display = event.size.height >= 40

    def tick(self):
        self.s.sample()
        self.refresh_views()

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
        g = self.query_one("#g_load")
        g.border_title = f"cpu  {u * 100:.0f}%"
        g.border_subtitle = "total load"
        g = self.query_one("#g_freq")
        g.border_title = f"freq  avg {avg:.0f}  ·  top {top:.0f} MHz"
        g.border_subtitle = f"0–{MAXF:.0f} MHz"
        wrap = self.query_one("#cores_wrap")
        view = "compact" if self.compact else "full"
        if VIEWS[self.view] == "auto":
            view = f"auto ({view})"
        wrap.border_title = f"cores  ·  sort: {key}  ·  view: {view}"
        wrap.border_subtitle = ("▀ upper half: first thread · lower half: second · " if self.compact
                                else "") + "↑↓←→ / click to select"
        threads = s.groups[self.selected]
        self.query_one("#detail").border_title = (
            f"C{self.selected:02}  ·  " + " / ".join(f"t{c:02}" for c in threads))
        self.query_one("#detail").border_subtitle = "frequency (top) · load (bottom)"

        for card in self.query(CoreCard):
            card.set_class(card.group() == self.selected, "selected")
            card.refresh()
        for w in ("#title", "#g_load", "#g_freq", "#sensors", "#detail"):
            self.query_one(w).refresh()

    def select(self, gi):
        self.selected = gi
        self.refresh_views()

    def action_move(self, dx, dy):
        i = self.order.index(self.selected)
        j = i + dx + dy * self.cols
        if 0 <= j < len(self.order):
            self.select(self.order[j])
            self.query(CoreCard)[j].scroll_visible()

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

    def action_reset_peaks(self):
        for c in CPUS:
            self.s.peak[c] = self.s.f(c)
        self.refresh_views()


if __name__ == "__main__":
    CpuView().run()

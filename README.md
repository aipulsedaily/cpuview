# cpuview

A btop-style terminal monitor that shows **every core's clock speed and load at once**, grouped
by physical core and CCD, with temperatures, history graphs, themes and picture backgrounds,
plus a **Memory & Storage** page (`m` or `2`) written for people who aren't Linux experts: how much
RAM is in use and whether that's fine, what the disk is doing now and did a minute ago, how full
each drive is, and which programs use the most memory, live; and a **Processes** page (`4`):
every process with its CPU, memory and disk use, as a list, grouped by program or as a tree, with
signals and nice. Native Rust, one 1.05 MB binary, ~4 MB of RAM (< 5 MB even at 4K), and under **0.01 % of one CPU
thread** (≈ 70–85 µs of CPU per second) at the default `eco` speed. Press `s`, `+` or `-` for
`smooth` (bars, digits and graphs all moving continuously at a steady 60 frames per second,
≤ 0.5 % of one thread) or fixed rates up to
60 updates per second.

![cpuview, Midnight theme, 128×45](docs/screenshots/rust-128x45.webp)

*128×45 (a ~1180×880 px Windows Terminal window), Midnight: graph band, all 32 threads in one column per
CCD with a load-history strip per thread, and the all-core load heatmap at the bottom. Real data on a
Ryzen 9 9950X with a synthetic load running. Captured headlessly: the app ran in a pseudo-terminal, its
output was replayed in a VT emulator (pyte) and painted with DejaVu Sans Mono.*

The layout adapts from 40×12 to 4K: every core stays visible, and bigger windows get more history
instead of longer bars. From a maximised 1080p window up (≈ 190×36 cells), a Memory & Storage
column sits beside the CPU view, so both are visible without switching.

![Memory & Storage page, 128×45](docs/screenshots/rust-memory-128x45.webp)

*`m`: the Memory & Storage page at 128×45. Each panel opens with its answer in the title ("RAM
31% in use · 9.91 of 32.2 GB · plenty free", "disk reading 117 MB/s · writing 15 MB/s · 3%
busy", "storage system (/) 9% full · 1.59 TB free"), coloured by health. Rows read like the CPU
view's (label → value → bar → %) with a short plain hint. History charts say "over time", have
units on the y-axis, "… ago · now" along the bottom and the current value at the right edge: RAM
in use (with "rising: 18% 1 min ago, 31% now"), and disk activity as stacked columns (reading
below, writing on top, each column a 2–5 s average) with totals and the peak in words. Then every
real filesystem, and the programs using the most memory, live. Captured during a synthetic CPU
and disk load.*

![Processes page, 128×45](docs/screenshots/rust-processes-128x45.webp)

*`4`: the Processes page at 128×45, smooth, grouped by program (`G`) and sorted by CPU (`o`). CPU %
is of one CPU thread (`I`: of the whole machine), memory is resident, disk is bytes read and
written per second (`?` where another user's process doesn't let us read it). The selected
process gets its whole command line, working directory, cgroup, and CPU and memory over the last
minute. The busy process is a test load (`videoenc`) running on this machine.*

![4K, 415x105, CPU view with the memory column](docs/screenshots/rust-4k-memory.webp)

*415×105 (4K at 100 %): the CPU view with the Memory & Storage column beside it.*

| | |
|---|---|
| ![4K, 275x70](docs/screenshots/rust-4k-275x70.webp) 275×70 (4K at 150 %): per-core history grid, memory column | ![172x40](docs/screenshots/rust-default.webp) 172×40, Midnight |
| ![60x20](docs/screenshots/rust-60x20.webp) 60×20: one row per core, live summary in the title | ![40x12](docs/screenshots/rust-40x12.webp) 40×12, Nebula: all 16 cores |
| ![Nebula](docs/screenshots/rust-nebula.webp) Nebula | ![Synthwave](docs/screenshots/rust-synthwave.webp) Synthwave Sunset |
| ![ISS aurora picture, sixel](docs/screenshots/rust-iss.webp) Picture theme (ISS aurora), sixel mode¹ | ![Night city picture, sixel](docs/screenshots/rust-city.webp) Picture theme (pixel night city), sixel mode¹ |
| ![Theme menu](docs/screenshots/rust-menu.webp) Theme menu with live preview | ![Smooth](docs/screenshots/rust-smooth.webp) `smooth` speed, mid-animation |
| ![120x30 Night Forest](docs/screenshots/rust-c120.webp) 120×30, Night Forest | ![80x24 compact](docs/screenshots/rust-compact.webp) 80×24 compact, Space |

¹ The sixel patches in these captures were decoded and composited by the capture harness
(with Windows Terminal's rule that text erases the image in its cell), not shown by a real
terminal.

## Features

- **Per-thread clock and load bars**, two rows per physical core (t03 and its SMT sibling t19):
  load is the thick bar, clock a thin gradient line; idle threads fade back. Wide windows add a
  **load-history strip** per thread (2 s per cell)
- **Grouped by CCD**: one column per L3 domain with that CCD's temperature in the header (a
  "CCD1 61° ───" divider row when one column holds both), per-CCD load and clock in the sensors
  panel, and ◆ marks the scheduler's preferred core(s) (highest amd-pstate ranking)
- **Adaptive layout**, all cores visible without scrolling from **40×12** up: full rows (one per
  thread), compact rows (one per core: core clock, one load bar whose faded part is the load only
  one thread reaches, both loads) or micro rows (bar, clock, busiest thread), one column per CCD
  where it fits; the graph band appears whenever there is room (one-row graphs on short windows) and
  the title shows a live load / clock / temperature summary when it doesn't
- **History**: braille graphs for total load and average clock; in the bottom area an **all-core
  load heatmap** (two cores per row, cool → hot) and, on big screens (4K), a **per-core history
  grid** — clock over load for every core; `d` switches to the selected core's per-thread graphs
- **Detail statistics** for the selected core: average / peak clock, time spent boosting, average
  load and the amd-pstate preferred-core rank
- **Sensors**: k10temp / zenpower / coretemp / cpu_thermal temperatures with their window maximum,
  governor and EPP, load average, busy and boosting thread counts, peak clock
- **10 themes** with procedural backgrounds (Midnight, Aurora, Deep Ocean, Ember, Mono, Nebula,
  Night Forest, Space, Synthwave Sunset, Terminal background) and **picture themes**
  (8 bundled wallpapers, your own JPEG/PNG files, or a URL)
- **Terminal graphics, detected over SSH**: sixel (Windows Terminal ≥ 1.22, foot, WezTerm,
  Konsole, xterm vt340, tmux ≥ 3.4), kitty, iTerm2 inline images, truecolor or 256-colour
  half-blocks, or the terminal's own background image
- Mouse: click a core (in the list or the history grid) to select it, click a footer key, wheel in
  the theme menu
- No root, no daemon: reads `/proc/stat` and sysfs; works on any Linux x86_64 / aarch64 and
  degrades gracefully without cpufreq, hwmon, SMT or CCD information

### Memory & Storage

Made to be read at a glance by someone who isn't a Linux expert: every panel was checked with
fresh readers who saw only a screenshot and had to answer what it says. Sizes are decimal (1 GB = 10⁹
bytes, like disk makers and `df -H`; the RAM total reads 32.2 GB on a "32 GB" machine), and
every rate is in MB/s, so all of them compare directly.

- **One answer per panel**, in its title and coloured by health (green fine, amber watch, red
  act): RAM "31% in use · 9.91 of 32.2 GB · plenty free" (amber from 80 % in use, or when
  programs wait for RAM ≥ 5 % of the time, or swap traffic ≥ 1 MB/s with less than 20 %
  available; red from 90 % / 20 % / 20 MB/s: "getting full", "nearly full", "RAM is tight"); disk
  "reading 117 MB/s · writing 15 MB/s · 3% busy", or "idle · nothing being read or written";
  storage "system (/) 9% full · 1.59 TB free" (amber from 90 %, red from 95 %); drive
  temperature OK / warm (70 °C) / too hot (80 °C), RAM sticks OK / warm (70 °C) / hot (85 °C).
- **RAM**: in use (what `free` calls used: total − available) with a bar and %, available ("free
  for programs now"), cache ("files kept for speed, part of available"), swap with a bar and
  whether it is active ("no activity (fine)", "little activity", "busy 3 MB/s"), "waiting" (PSI
  `some avg10`: the share of time programs waited for RAM), RAM stick temperatures (`spd5118` /
  `jc42`). A chart of **RAM in use over time** (0–100 %, a line with a soft fill, no cache slab)
  says the trend in words: "rising: 18% 1 min ago, 31% now".
- **Disk activity** (physical disks only: no partitions, loop, dm, md, zram, optical or eMMC boot
  areas): reading ("programs loading files") and writing ("programs saving files") in MB/s over
  ~1 s, busy % (time the disk was working), temperature (`nvme` / `drivetemp`) and model. The
  **disk activity over time** chart is a stacked column chart, not a waveform: each column is
  the average of 2–5 s (aligned so columns step once per bucket and never change in between),
  reading below and writing on top in two colours the theme can't make alike, a round MB/s
  scale (1-2-3-5 steps, 10 MB/s at least, so an idle disk is a flat line labelled "idle"), the
  current rates at the right edge, and in words: "read 95 GB · written 1.9 GB in the last 2 min ·
  peak 2731 MB/s 45 s ago · 1 min ago 2650, now 133 MB/s". Three or more disks: one line each and
  one chart of all of them.
- **Storage**: every real filesystem once (no tmpfs, snaps, overlays, network mounts or bind
  duplicates): "1.59 TB free", a bar, "9% full" (`df`'s use %), then "system & home · 152 GB used
  of 1.84 TB · ext4 · device".
- **Top programs by memory**, live: processes grouped by program ("postgres ×27 613 MB": each
  process's private memory plus the largest shared part once, since shared libraries and shared
  memory sit in RAM once); `p` shows each process instead. Rows keep their place unless another
  is clearly bigger (3 %, 2 MB), and in smooth mode cross-fade when they trade places; sizes
  count smoothly.
- Small windows keep the answers and drop charts first: a chart becomes a one-line history with
  words ("1 min ago ▁▂▃▅ now · rising 19% → 31%", "· peak 2723 MB/s 46 s ago"), storage can
  shrink to the root filesystem ("+ 2 more"). From 100 columns the page has two columns; from ≈
  190×36 the column joins the CPU view.
- Works on any Linux machine: no swap ("swap none · no swap set up"), zram swap ("swap zram"),
  no sensors (no temperature rows), no PSI (no "waiting" row), NVMe / SATA / HDD / eMMC / virtio,
  any number of disks and filesystems, long mount paths.

### Processes

- **Every process**: PID, user, CPU % (from `utime + stime` over the last second, of one CPU
  thread or, with `I`, of the whole machine), resident memory and % of RAM, disk read / write
  per second (`/proc/<pid>/io`), threads, state and age; the command line after the name.
  Columns drop out as the window narrows (age, disk, threads, state, % RAM, bar, user, PID)
- **Sort** by CPU, memory, disk, PID or name (`o`, `r` reverses, or click the column titles);
  near-ties don't flip-flop: a new value counts at once past a margin (CPU: 2.5 points or 20 %,
  memory 2 %), otherwise only once it has lasted 4 s
- **Group by program** (`G`, on by default): `postgres ×31` with summed CPU, memory, disk and
  threads; `enter` / `→` expands a group, `←` folds it. **Tree** (`T`): children under their
  parents, foldable the same way. **Filter** (`/`): name, command line, user or PID; `esc`
  clears it
- **Detail strip** for the selected process (or group): whole command line, parent, user, state,
  threads, nice, age, swap, the PID inside its container, working directory, cgroup, and CPU and
  memory charts for the last 60 s
- **Signals** (`k` or `F9`: TERM, KILL, HUP, INT, STOP, CONT) and **nice** (`n`) go through a
  dialog and never happen without `enter`; a process is identified by PID *and* start time (and
  signalled through a pidfd), so a reused PID is never hit; refusals are spelled out
  (another user's process, raising priority without root, already exited)
- **Any Linux, any user**: with `/proc` mounted `hidepid=2` you see your own processes and a note
  says why; unreadable files show `?`; kernel threads are counted and hidden (`K` shows them);
  zombies show `<defunct>`; long command lines are cut with `…`; names with spaces and
  parentheses parse; thousands of processes are fine (~150 bytes each: under 5 MB of RAM with
  5,400 processes); user names from `/etc/passwd`, LDAP / sssd users through the system's user
  database; no systemd needed
- **Live and smooth**: in smooth mode the rows on screen are re-read 2.5 times a second (their
  `stat` and `statm` stay open), CPU % is over the last second, digits count and bars glide,
  rows *slide* to a new place over ~0.3 s instead of jumping, new processes fade in and exited
  ones fade out. In eco the rows shown are re-read every 4 s and the whole list every 30 s

## Install

Needs a Rust toolchain (<https://rustup.rs>) on Linux.

```sh
git clone https://github.com/aipulsedaily/cpuview.git
cd cpuview
./install.sh        # builds, installs ~/.local/bin/cpuview and the wallpaper pack
cpuview
```

`install.sh` runs `cargo build --release`, copies the binary to `~/.local/bin/cpuview` and the
wallpapers to `~/.local/share/cpuview/wallpapers/`. If an older Python `cpuview` launcher is
there it is kept as `~/.local/bin/cpuview-py`. The previous Python/Textual version lives in
`legacy/python/` and still runs (`legacy/python/install.sh`).

Uninstall: `rm ~/.local/bin/cpuview; rm -r ~/.local/share/cpuview ~/.config/cpuview`.

## Keys

| Key | Action |
|---|---|
| `↑↓←→` / `hjkl` / click | select a core (columns run top to bottom) |
| `s` | update speed: eco → smooth → 1 → 2 → 4 → 10 → 20 → 30 → 60 per second → eco |
| `1` `2` `3` `4` / `tab` / `shift-tab` | pages: CPU, Memory & Storage, Network, Processes (remembered; `--page cpu\|memory\|network\|processes`) |
| `m` | CPU view ⇄ Memory & Storage page |
| `p` | Memory & Storage: top programs by memory ⇄ each process |
| `+` / `-` | faster / slower (`=` works as `+`) |
| `o` | sort cores: number → load → clock |
| `c` | view: auto → full → compact |
| `g` | show / hide the history graphs |
| `d` | bottom area: all-core history (heatmap / grid) → selected core's graphs → hidden |
| `enter` | show / hide the bottom area |
| `r` | reset peak clocks |
| `space` | pause (stops sampling entirely) |
| `t` | theme menu |
| `q` / `ctrl-c` | quit (`ctrl-z` suspends) |

On the Processes page (`4`) the arrows, `pgup` / `pgdn` / `home` / `end`, the mouse wheel
and clicks move the selection, and these keys change meaning (the global ones — `q`, `space`,
`s`, `+` `-`, `t`, `1`–`4`, `tab` — still work):

| Key | Processes page |
|---|---|
| `o` / `r` | sort column: CPU → memory → disk → PID → name / reverse |
| `/` | filter by name, command, user or PID (`enter` keeps it, `esc` clears it) |
| `G` / `T` | group by program (default) / tree |
| `enter` / `→` / `←` | expand / fold the selected group or branch |
| `k` or `F9` | send a signal (TERM, KILL, HUP, INT, STOP, CONT): choose with `←→`, `enter` sends, `esc` cancels |
| `n` | change nice (`←→` or `-` `+`, `enter` applies) |
| `I` | CPU % of one CPU thread ⇄ of the whole machine |
| `K` | show / hide kernel threads |

`k` is "up" on the CPU page (vim keys); on the Processes page it opens the signal dialog, as in
`top` and `htop`, which is safe because nothing happens before `enter`.

Theme menu: `↑↓` preview live, `enter` apply, `esc` cancel, `←→` picture brightness,
`b` blur, `m` image mode, `a` add a picture (path or URL).

Speed is shown in the title bar (`● live eco`, `● live smooth`, `● live 10/s`) and the footer,
and is remembered in `~/.config/cpuview/config`; `cpuview --speed smooth` (or `10`, …) sets it
from the command line. Except in eco the title clock shows seconds; history graphs always get
one point per second.

**Smooth** is made to look live: everything that changes moves continuously instead of
jumping, on a fixed frame grid.

- **Frames**: a steady 60 frames per second on a fixed clock grid (the 10 samples a second fall
  on it), one write per frame wrapped in synchronized-output marks, nothing written when nothing
  changed. Every frame advances the animation by exactly one grid step, so motion per frame is
  even whatever the wakeup jitter. Every other frame touches only the core cards (bars and
  their digits); panels and graphs change on the others.
- **Bars** follow critically damped springs (no overshoot) whose targets move 10 times a
  second; with the spring slow against the sample rate the motion is C1-continuous (no
  start-stop pulses: velocity ripple under ~10 %). The bar end is anti-aliased in 1/16 cell
  steps, blended in linear light so it glides at an even perceived speed across a cell.
- **Load** blends two windows of `/proc/stat` (which counts in 10 ms ticks): 0.8 s (±1 %) while
  it agrees with 0.2 s (±5 %) within the noise, the short one as they disagree. Quiet bars stay
  still; a step 10 % → 80 % reaches 90 % in ~0.45 s. Changes below ±0.8 % load / ±1.5 % clock
  don't move a bar. A disagreement within the short window's noise counts only if the sample
  before agreed in sign, and once a step or ramp has ended the bar settles on a 0.4 s window:
  ramp ends no longer overshoot (they did by ~3 %, about 0.4 cell, before settling).
- **Digits** count along with their bar (90 → 91 → … → 99), with hysteresis while it moves so a
  value near a rounding edge never flickers; exact once it rests. Totals, CCD figures and
  temperatures (read every second, eased over ~1 s) do the same; colours follow the digits.
- **Graphs** keep every per-second point on its own braille dot column and move it one column
  when the next point arrives; only the newest column is live (the number as shown, held to
  whole dots, so it can't flicker at a dot's edge). An earlier version slid the points by the
  sub-second phase, which made every column alternate between a point and the mean of two
  neighbours once a second: one-point peaks shrank and regrew and one-dot features blinked off
  and on. Checked frame by frame: no cell changes and returns within 6 frames in 40 s runs at
  128×45 and 172×40, CPU view and Memory & Storage, Midnight and a picture theme.
- **Pictures**: sixel/iTerm pixel patches are sent only with full repaints (start, keys,
  resize), never mid-animation; a cell that turns blank in between shows the half-block
  backdrop until then.

Measured from frame-by-frame captures at 128×45 (one thread driven by a 10 ms duty-cycle load,
bar length read back from the edge cell's colour), previous smooth → now: frame intervals p95 /
median 1.95 → 1.01–1.02 and longest gap 80–100 → 19 ms while bars move; a 10 % → 80 % step reaches
90 % in 0.80 → 0.44 s with no overshoot; on a 3 s ramp the share of frames where the bar stands
still 29–41 % → 12–15 % (the rest of those are sub-1/16-cell steps) and velocity variation
(50 ms bins) 0.58–0.63 → 0.35–0.40; graphs update 0.5 → 4–6 times a second with ≤ 2 cells a
frame instead of a 20–35-cell jump every 2 s; quiet threads' digits change 0.3–0.6 times a
second (max 1.8), bar reversals unchanged (≤ 1.2/s).

Clocks in smooth mode are the clock *while running*: see
[Load and clock accuracy](#load-and-clock-accuracy). Should frames cost more than 5 ms of CPU per
second for 3 s in a row (huge windows), the frame rate halves, and stays constant from there;
`CPUVIEW_FPS=30` sets it.

## Themes and pictures

```sh
cpuview --theme nebula
cpuview --image ~/Pictures/wall.jpg              # copied to ~/.config/cpuview/images/
cpuview --image https://example.org/photo.jpg    # downloaded with curl (15 s, 25 MB max)
```

Any JPEG or PNG in `~/.config/cpuview/images/` shows up in the theme menu. From Windows
PowerShell:

```powershell
scp .\pic.jpg you@yourserver:~/.config/cpuview/images/
```

A picture theme takes its palette from the picture (load and temperature colours keep their
meaning). Behind the panels the picture is blurred and tinted so the numbers stay readable over
busy pictures; the bare screen between panels shows it sharp. The theme menu shows the credit of
the bundled pictures (`wallpapers/CREDITS.md`: NASA public-domain photos and CC0 scenes).
Pictures stay on disk; the binary embeds only the theme definitions (`themes/*.toml`, compiled in
by `build.rs`).

## Image modes and `--doctor`

`--image-mode auto|kitty|sixel|iterm|blocks|blocks256|terminal|off` (remembered; `m` in the menu).
`auto` asks the terminal what it can do (works over SSH, where `TERM_PROGRAM` and `COLORTERM` are
usually not forwarded) and picks kitty → sixel → iTerm2 → truecolor half-blocks → 256 colours →
off. Truecolor is detected with a DECRQSS echo, not from the environment. The probe never waits
more than 300 ms (1 s once replies start arriving) and late replies are ignored by the input
parser.

```
$ cpuview --doctor
terminal      (no XTVERSION reply)
TERM          xterm-256color   COLORTERM=(unset)   ssh=yes
replies       yes in 41 ms   DA1=[61, 4, 6, 7, 14, 21, 22, 23, 24, 28, 32, 42, 52]
truecolor     yes (DECRQSS echoed)
sixel         yes
cell size     10x20 px (sixel default (VT340 / Windows Terminal virtual cell))
→ image mode  sixel  (DA1 lists 4 (sixel))
```

(that is what a Windows Terminal 1.22 reply looks like; `--doctor` then draws a test strip in
the chosen mode). Pixel data is sent only on start, resize and theme change: sixel / iTerm2
patches cover blank cells and survive live redraws because unchanged cells are never rewritten.

**Windows Terminal background image**: `cpuview --theme transparent` leaves every cell at the
terminal's default background so a `backgroundImage` set in the Windows Terminal profile shows
through (setup in [docs/image-themes.md](docs/image-themes.md)).

## Load and clock accuracy

`/proc/stat` counts CPU time in USER_HZ ticks of **10 ms**. Sampled 60 times a second, a CPU
gets only one or two ticks per interval, so its load can only read 0, 50 or 100 % — a thread
running 3 ms of every 10 ms flickered between 0 and 100 %, and idle threads jumped to 100 %
whenever they ran briefly. cpuview therefore computes load over a **sliding 0.5 s window** at
every speed (a ring of `/proc/stat` snapshots; the base is the newest one at least 0.5 s old,
so slower speeds simply use the previous sample): ≥ 48 ticks per CPU, ±2 points. Checked with
a pinned 3 ms busy / 7 ms sleep load (true duty 29.9 %, confirmed by the CPU's run time in
`/proc/schedstat`), displayed value of that thread over 8 s:

| speed | before | now |
|---|---|---|
| 60/s | 0 – 100 % | 29 – 31 % |
| 10/s | 22 – 30 % | 28 – 31 % |
| smooth | – | 29 – 30 % |

Before, at 60/s, threads that `/proc/schedstat` showed at most 5 % busy jumped to 50–100 %;
now every idle thread's peak matches its real run time within a few points.

Clocks come from `scaling_cur_freq`, which for a lightly loaded thread flips between the idle
floor (0.62 GHz here: the thread happened to be idle at its last tick) and its running clock
from one read to the next. Fixed speeds show the raw reading. Smooth shows the clock while
running: readings above the floor are averaged and held for 1.5 s, and a thread drops to the
floor only after 1.5 s without one.

## How light is it

The default `eco` speed is the one held to 0.01 %; the others trade CPU for liveliness
(172×40, Midnight, output to a continuously read pseudo-terminal, load average 2–3 from other
work; smooth and eco 60 s runs, fixed speeds 20 s):

| speed | samples/s | frames/s | CPU per second | % of one thread | output |
|---|---|---|---|---|---|
| eco (default) | ~0.5–1 | ~0.4 | 83 µs | 0.0083 % | 2.0 KB/s |
| smooth | 10 | 60 (steady; fewer when nothing moves) | 2.7 ms | 0.27 % | 33 KB/s |
| 1/s · 2/s · 4/s | 1 · 2 · 4 | same | 0.17 · 0.32 · 0.46 ms | 0.017 · 0.032 · 0.046 % | 3–13 KB/s |
| 10/s | 10 | ~9 | 0.89 ms | 0.089 % | 7 KB/s |
| 20/s · 30/s · 60/s | 20 · 30 · 60 | ~18 · 27 · 51 | 2.0 · 2.7 · 5.2 ms | 0.20 · 0.27 · 0.52 % | 31 · 25 · 68 KB/s |

Smooth, 60 s runs at 128×45 · 172×40: Midnight 2.2 · 2.7 ms/s and 30 · 33 KB/s; NASA Webb
picture (sixel) 3.4 · 3.6 ms/s and 38 · 27 KB/s; ≤ 3.3 KB per frame, never a picture patch after
the first paint. (The previous smooth: 1.0 – 2.3 ms/s at ~20 – 50 irregular frames/s.) Its cost
is mostly the 60 wakeups a second: on an idle machine a bare wakeup of a cache-cold core costs
20–30 µs (3–6 µs when the CCD is busy). Fixed-speed
cost depends mostly on how many cells change, so a busier machine costs more.

Sample rates were confirmed by counting `/proc/stat` reads with strace. Details for eco:

Measured on this machine (Ryzen 9 9950X, Linux 7.0, output going to a pseudo-terminal that is
continuously read, 60 s after a 10 s warm-up, CPU time = sum of `/proc/<pid>/task/*/schedstat`
field 1 in nanoseconds), while another workload kept the load average at 1.4–3.7:

| 60 s run | CPU per second | % of one thread | RSS |
|---|---|---|---|
| Midnight, CPU view: 128×45 · 415×105 (4K, with the memory column) | 82.1 · 95.8 µs | ≤ 0.0096 % | 3.9 · 4.9 MB |
| Midnight, Memory & Storage page: 128×45 · 415×105 | 83.5 · 90.6 µs | ≤ 0.0091 % | 4.1 · 4.8 MB |
| the same, the build before the clearer memory page: CPU view · memory page 128×45 | 82.1 · 82.1 µs (load 2.3) | ≤ 0.0083 % | 4.1 MB |
| Midnight, the build before the memory page: 128×45 · 172×40 · 415×105 | 83.6 · 82.7 · 77.4 µs | ≤ 0.0084 % | 3.8 · 3.9 · 4.7 MB |
| Midnight (default), 172×40, earlier builds | 68.6 · 71.3 · 82.1 µs | ≤ 0.0082 % | 3.9 MB |
| Space · ISS aurora picture (sixel) · Midnight 120×30, earlier build | 73.4 · 63.8 · 73.3 µs | ≤ 0.0073 % | 4.0 · 7.2 · 3.8 MB |

| Midnight, Processes page (~130 processes, load 6–7): 128×45 · 415×105 | 68.3 · 68.6 µs | ≤ 0.0069 % | 4.0 · 5.0 MB |

Processes page, smooth, 60 s at 128×45 · 172×40 with a load average of 7: 2.8 · 2.8 ms/s, 18 · 22
KB/s, frames on the 60 fps grid (the base Memory & Storage page under the same load: 3.2 ms/s).
With 5,400 processes it stays at 4.7 MB of RAM (128×45). A full scan of every process's `stat`
costs ~1–1.5 ms here, so smooth does one about every 1–4 s (spaced by its measured cost), eco
every 30 s or more; nothing is read for the page while it isn't shown.

(The rows above were measured while other monitors and builds kept the load average at 3.6–3.8.)

Smooth, 40 s runs at 128×45 · 172×40 under the same load: Memory & Storage page Midnight 3.4 ·
3.5 ms/s and 7 · 7 KB/s, NASA Webb picture (sixel) 3.2 · 3.3 ms/s and 11 · 10 KB/s; CPU view 3.7 ·
3.9 ms/s and 66 · 73 KB/s, 4.2 · 4.2 ms/s and 53 · 52 KB/s; frames stay on the 60 fps grid
(consecutive frame intervals p95 / median 1.03–1.04, ≤ 0.1 % off the grid, no picture data after
the first paint).

Binary: 979,432 bytes (`opt-level = "z"`, fat LTO, one codegen unit, `panic = "abort"`,
stripped; `"s"` is 13 KB larger and not measurably faster here, `2`/`3` exceed 1 MB). The memory
page fits in less than before: no float formatting code at all (a non-constant `f32::clamp`
pulled in ~30 KB of it for its panic message), short lists sorted by one small insertion sort
instead of std's ~4 KB sort per element type, ASCII lower-casing instead of Unicode tables, and
the picture download runs `curl` through `posix_spawnp` instead of `std::process::Command`
(~33 KB less).
The Processes page adds about 68 KB the same way, plus: one non-generic heap sort, string
tables kept out of the binary's relocations (every `&str` in a constant table costs 40 bytes in a
position-independent binary; a `match` costs none), and no `core::fmt` for numbers.

**Memory & Storage data** is read the same way, with every reading's cost in mind (cold cache,
this machine): `/proc/meminfo` and each disk's sysfs `stat` (the `/proc/diskstats` counters for
that disk alone: 0.9 µs instead of 8 µs for the whole table) every sample, every third while
the page is hidden so the histories keep going; `/proc/vmstat` (swap in/out, 10 µs) and PSI at
most every 4 s in eco (2.5 times a second in smooth); `statvfs` every 10 s (3 s); the mount table
only when `poll()` on `/proc/self/mountinfo` reports a change. A full process scan costs ~1.3 µs
a process (names are read once per pid, kernel threads remembered and skipped), so it runs every
4 s (eco: 2 minutes); in between the 32 biggest processes are followed live through their open
`statm` (one `pread`, ~1 µs each) 2.5 times a second (eco: every 10 s), and eco re-scans early
when RAM in use changes by more than those processes explain or, every 30 s, when a new pid of
64 MB or more appears. DIMM (`spd5118`, SMBus)
and NVMe temperatures cost 40–65 µs of CPU and 1–20 ms of *wall* time per read, so a helper
thread reads them every 60 s in eco (5 s otherwise) and only while they're shown: the main loop
never waits for a sensor, and smooth frames stay on time.

What makes that possible:

- The process sleeps in `ppoll()` and wakes only to sample, at the minute boundary (title clock)
  or on input. One wakeup = `pread()`s on file descriptors opened once + one `write()`.
- On a core that has been idle, everything is cache-cold: a bare wakeup costs ~19 µs, reading
  `/proc/stat` ~45 µs (the kernel sums every IRQ counter of every CPU) and each
  `scaling_cur_freq` ~1.4 µs. So a full sample once per second would already cost ~110 µs/s.
  The sample rate is therefore paced by a **CPU-time budget**: a token bucket refills at
  70 µs per second (capped at 1.2 ms), each wakeup pays its measured CPU time, and samples come
  every second while loads change and every 2 s when they are stable — never faster than the
  budget allows. So the average stays at or below 70 µs/s on any machine and any 60 s window
  stays below 90 µs/s. In the runs above that was one update every ~2 s on average, with 1 s
  bursts when something changes (start-up, load spikes). Key presses are answered immediately
  and full repaints after input are not charged to the budget.
- The process is pinned to the physical core the scheduler uses least (lowest amd-pstate
  preferred-core rank, else the last core) among the CPUs it may use — a single-core
  `taskset` is left alone (`CPUVIEW_PIN=off` disables): that core's private L2 usually still holds cpuview's code and
  data at the next wakeup, ~15 % cheaper per sample.
- Clocks are read only for CPUs that did work in the load window (idle CPUs report their
  minimum clock anyway); barely busy CPUs are read round-robin; SMT siblings that are both
  busy share one read (one clock domain). One temperature sensor every 3 s (k10temp reads are
  ~10 µs), load average every 5 s, governor every 10 s.
- Rendering is retained: cells are compared as they are written, so only changed cells are
  composited and sent; bars update only the cells between the old and new length; graphs,
  strips and the heatmap scroll every 2 s at eco and fixed speeds (one braille cell; every 4–6 s on very large windows,
  where most frames then only touch bars and numbers); integers and decimals are formatted without
  `core::fmt` (no float formatting or parsing code in the binary); big buffers live in their own
  mappings and go back to the kernel when freed. Smooth's
  animation frames redraw only what moved (memo keys per panel and per card row) and never
  send picture data.

## Notes

- Clock speeds come from cpufreq (`scaling_cur_freq`), like the Python version.
- Ryzen reports temperature per CCD (`Tctl`, `Tccd1`, `Tccd2`), shown above each CCD column.
- Avoids glyphs Cascadia Mono lacks (the best preferred core is marked `◆`).
- Screenshots in `docs/screenshots/theme-*.webp`, `image-*.webp`, `compact-80x24.webp` and
  `terminal-background-wt.webp` are the **design mockups** made before the Rust version (demo
  data, Python reference compositor). `docs/screenshots/rust-*.webp` are captures of this
  version. Sixel pictures in those captures are decoded by the capture harness, not by a real
  terminal.

## Development

```sh
cargo test --release         # parsing (/proc/stat, meminfo, vmstat, diskstats, mountinfo), layout fit at many sizes, probe matrix, sixel round trip, input parser …
cargo clippy --release
cargo build --release --features prof && CPUVIEW_PROFILE=/tmp/prof.txt target/release/cpuview  # per-phase CPU time, written on exit
python3 themes/reference.py render nebula out.png   # the theme spec's reference renderer
```

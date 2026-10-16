//! Dashboard layout and drawing (port of the Python/Textual widgets).

use crate::prof::prof;
use crate::screen::*;
use crate::sys::Sampler;
use crate::theme::*;
use std::fmt::Write as _;

#[path = "memui.rs"]
mod memui;
pub use memui::*;
// view 4: processes (and the page numbers / tabs shared by all views)
#[path = "procui.rs"]
mod procui;
pub use procui::*;

const SLOT_TITLE: usize = 0;
const SLOT_GLOAD: usize = 1;
const SLOT_GFREQ: usize = 2;
const SLOT_SENSORS: usize = 3;
const SLOT_HEADER: usize = 4;
const SLOT_DETAIL: usize = 5;
const SLOT_GBODY: usize = 6;
const SLOT_DBODY: usize = 7;
const SLOT_SDYN: usize = 8;
const SLOT_SHIST: usize = 9;
const SLOT_STATS: usize = 10;
const SLOT_GBODY2: usize = 11;
const SLOT_CARDS: usize = 12;

/// Memo key: a small mixing hash of display-relevant values.
pub fn mixk(v: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in v {
        h = (h ^ x).wrapping_mul(0x100_0000_01b3);
        h ^= h >> 29;
    }
    h
}

/// Per CCD: (mean load of its threads, mean core clock in MHz, threads ≥ 50 %).
pub fn ccd_stats(s: &Sampler, d: usize) -> (f32, f32, usize) {
    let (mut u, mut f, mut nt, mut nc, mut busy) = (0.0f32, 0.0f32, 0usize, 0usize, 0usize);
    for (g, thr) in s.t.groups.iter().enumerate() {
        if s.t.ccd[g] != d {
            continue;
        }
        nc += 1;
        f += thr.iter().map(|&c| s.f[c]).fold(0.0, f32::max);
        for &c in thr {
            u += s.u[c];
            nt += 1;
            busy += (s.u[c] >= 0.5) as usize;
        }
    }
    (u / nt.max(1) as f32, f / nc.max(1) as f32, busy)
}

fn sensors_key(s: &Sampler) -> u64 {
    let mut k = 0u64;
    for d in 0..s.t.ndom {
        let (u, f, _) = ccd_stats(s, d);
        k = mixk(&[k, (u * 100.0) as u64, (f / 10.0) as u64]);
    }
    for t in &s.temps {
        k = mixk(&[k, (t.val * 10.0) as u64]);
    }
    let busy = s.u.iter().filter(|&&u| u >= 0.5).count() as u64;
    let boost = s.f.iter().filter(|&&f| f >= s.t.boost).count() as u64;
    let peak = s.peak.iter().copied().fold(0.0, f32::max);
    let strs = s
        .gov
        .bytes()
        .chain(s.epp.bytes())
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64));
    mixk(&[
        k,
        (s.loadavg[0] * 100.0) as u64,
        (s.loadavg[1] * 100.0) as u64,
        (s.loadavg[2] * 100.0) as u64,
        busy,
        boost,
        (peak / 10.0) as u64,
        strs,
    ])
}

pub const SORTS: [&str; 3] = ["core", "load", "freq"];
pub const VIEWS: [&str; 3] = ["auto", "full", "compact"];
/// Update speeds: (label, samples per second). 0 = eco: adaptive 1–2 s, held under
/// 0.01 % of one thread by the CPU-time budget in the main loop. Smooth samples at
/// 10/s and animates everything on a fixed 60 frames/s grid (see `anim`).
pub const SPEEDS: [(&str, i64); 9] = [
    ("eco", 0),
    ("smooth", 10),
    ("1/s", 1),
    ("2/s", 2),
    ("4/s", 4),
    ("10/s", 10),
    ("20/s", 20),
    ("30/s", 30),
    ("60/s", 60),
];

pub const SMOOTH: usize = 1;

/// Smooth-mode target deadbands (fractions of the bar)
pub const DEADBAND_U: f32 = 0.008;
pub const DEADBAND_F: f32 = 0.015;

/// Bar fill in sixteenths of a cell (the edge cell's colour is blended by coverage).
#[inline]
pub fn fill16(frac: f32, w: usize) -> usize {
    (frac.clamp(0.0, 1.0) * (w * 16) as f32 + 0.5) as usize
}

/// Smooth mode: indices into `App::hy` (shown numbers) after the per-CPU ones.
const HY_TOT: usize = 0;
const HY_AVG: usize = 1;
const HY_TOP: usize = 2;
const HY_TEMP: usize = 3;

/// History series a graph, strip or heatmap shows.
#[derive(Clone, Copy)]
pub enum Hist<'a> {
    Tot,
    Avg,
    Temp(usize),
    /// mean load of these CPUs
    Load(&'a [usize]),
    /// highest clock of these CPUs
    Clock(&'a [usize]),
}

/// "eco", "10/s", "10" (and "auto" for eco) → index into SPEEDS; unknown → eco.
pub fn speed_index(v: &str) -> usize {
    let v = v.trim().trim_end_matches("/s");
    if v == "auto" {
        return 0;
    }
    SPEEDS
        .iter()
        .position(|s| s.0.trim_end_matches("/s") == v)
        .unwrap_or(0)
}
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const BRAILLE: [(u32, u32); 4] = [(0x40, 0x80), (0x04, 0x20), (0x02, 0x10), (0x01, 0x08)];

/// Stack-allocated formatter (no heap allocation on the render path).
pub struct F {
    b: [u8; 120],
    pub n: usize,
}
impl F {
    pub fn new() -> F {
        F { b: [0; 120], n: 0 }
    }
    pub fn s(&self) -> &str {
        std::str::from_utf8(&self.b[..self.n]).unwrap_or("")
    }
}
impl std::fmt::Write for F {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let mut take = s.len().min(self.b.len() - self.n);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.b[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        Ok(())
    }
}
#[macro_export]
macro_rules! f {
    ($($t:tt)*) => {{ let mut b = $crate::ui::F::new(); let _ = write!(b, $($t)*); b }};
}

impl F {
    pub fn push(&mut self, b: u8) {
        if self.n < self.b.len() {
            self.b[self.n] = b;
            self.n += 1;
        }
    }
    pub fn str(&mut self, s: &str) {
        let _ = std::fmt::Write::write_str(self, s);
    }
    /// Unsigned integer, right-aligned to `width` (no core::fmt).
    pub fn int(&mut self, v: u32, width: usize) {
        let mut d = [0u8; 10];
        let (mut i, mut v) = (10, v);
        loop {
            i -= 1;
            d[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for _ in (10 - i)..width {
            self.push(b' ');
        }
        for &c in &d[i..] {
            self.push(c);
        }
    }
}

/// "5.53": MHz as GHz with two decimals (fixed point, no float formatting).
pub fn ghz(mhz: f32) -> F {
    let c = (mhz.max(0.0) / 10.0).round() as u32;
    let mut f = F::new();
    f.int(c / 100, 1);
    f.push(b'.');
    f.push(b'0' + (c / 10 % 10) as u8);
    f.push(b'0' + (c % 10) as u8);
    f
}

/// Rounded integer with optional prefix/suffix, right-aligned to `width`.
pub fn num(pre: &str, v: f32, width: usize, suf: &str) -> F {
    let mut f = F::new();
    f.str(pre);
    f.int(v.max(0.0).round() as u32, width);
    f.str(suf);
    f
}

pub fn span(secs: usize) -> F {
    let (m, s) = (secs / 60, secs % 60);
    if m > 0 {
        f!("{m}m {s:02}s")
    } else {
        f!("{s}s")
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}
impl Rect {
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    pub fn inner(&self) -> Rect {
        Rect {
            x: self.x + 2,
            y: self.y + 1,
            w: self.w.saturating_sub(4),
            h: self.h.saturating_sub(2),
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct Layout {
    pub top: Option<Rect>,
    pub g_load: Rect,
    pub g_freq: Rect,
    pub sensors: Rect,
    pub cores: Rect,
    pub det: Option<Rect>,
    pub compact: bool,
    /// M_FULL / M_COMPACT / M_MICRO
    pub mode: u8,
    pub cols: usize,
    pub nrows: usize,
    pub rh: usize,
    pub header: bool,
    pub gap: bool,
    #[allow(dead_code)]
    pub fits: bool,
    /// x offset (relative to the cores' inner rect) and width of each card column
    pub card_x: Vec<(usize, usize)>,
    pub vis_rows: usize,
    /// the detail area shows every core's history (small multiples)
    pub grid: bool,
    /// a column holding two CCDs gets a divider row at the boundary ("── CCD1 61° ──")
    pub div: bool,
}

pub const M_FULL: u8 = 0;
pub const M_COMPACT: u8 = 1;
pub const M_MICRO: u8 = 2;
/// minimum card width and column gutter per card mode
const MINW: [usize; 3] = [46, 36, 15];
const GUT: [usize; 3] = [3, 3, 3];
/// smallest graph band / detail panel worth showing
const TOP_MIN: usize = 7;
/// a graph band this short (one graph row) only when nothing else fits
const TOP_MINI: usize = 3;
const DET_MIN: usize = 7;
/// seconds of history per cell of a core's load strip (same as the graphs)
pub const STRIP_SECS: usize = 2;
/// width of the selected core's statistics beside the history grid
const GRID_STATS: usize = 30;
/// bar glyphs: clock bars are thinner than load bars (load is what changes)
/// The memory column joins the CPU view from this size (1080p maximised ≈ 205×50).
pub const SIDE_W: usize = 190;
pub const SIDE_H: usize = 36;
pub const CLK: char = '▂';
pub const LOAD: char = '▄';

#[derive(Clone, Copy, Debug)]
struct Plan {
    mode: u8,
    cols: usize,
    nrows: usize,
    rh: usize,
    header: bool,
    top: bool,
    det: bool,
    /// rows left after the cores and their header
    rest: usize,
}

/// Field positions inside a core card (offsets from the card's left edge) and widths.
#[derive(Clone, Copy, Default, Debug)]
pub struct Geo {
    /// clock bar, load bar and load-history strip widths
    pub fw: usize,
    pub lw: usize,
    pub sw: usize,
    /// clock bar, clock digits, load bar, load digits, strip
    pub xb: usize,
    pub xn: usize,
    pub xl: usize,
    pub xp: usize,
    pub xs: usize,
    /// micro cards: room for a one-decimal clock
    pub ghz: bool,
}

pub struct App {
    pub s: Sampler,
    pub t: Theme,
    pub sort: usize,
    pub view: usize,
    /// index into SPEEDS
    pub speed: usize,
    pub paused: bool,
    pub show_top: bool,
    pub show_det: bool,
    /// detail area shows the selected core's graphs instead of the all-core overview
    pub det_core: bool,
    pub selected: usize,
    pub order: Vec<usize>,
    pub lay: Layout,
    pub w: usize,
    /// width of the CPU view (less than `w` with the memory column beside it)
    pub cw: usize,
    pub h: usize,
    pub zones: Vec<(usize, usize, char)>,
    pub scroll: usize,
    pub clock: (i32, i32, i32),
    pub uptime: u64,
    pub vals: Vec<f32>,
    pub vals2: Vec<f32>,
    pub tmp16: Vec<u16>,
    pub tmp8: Vec<u8>,
    pub idw: usize,
    pub rh_full: usize,
    pub menu: Option<crate::menu::Menu>,
    pub cache: Vec<u64>,
    pub prev_order: Vec<usize>,
    /// per thread: last drawn field keys (freq bar, clock, load bar, %, trend, name)
    pub row_memo: Vec<[u32; 6]>,
    pub title_short: std::cell::Cell<bool>,
    pub mode_line: String,
    pub depth_name: &'static str,
    /// bar positions per CPU (fractions 0..1: clock of maxf, load) and, in smooth
    /// mode, their spring velocities (per second)
    pub bf: Vec<f32>,
    pub bu: Vec<f32>,
    vf: Vec<f32>,
    vu: Vec<f32>,
    /// smooth mode: where the bars are heading (the latest sample)
    tf: Vec<f32>,
    tu: Vec<f32>,
    /// smooth mode: the numbers shown (see `shown`)
    nf: Vec<f32>,
    nu: Vec<f32>,
    /// smooth mode: temperatures (position, velocity) and the other shown numbers
    /// (HY_* then one per sensor, then per CCD load %, then per CCD clock)
    tt: Vec<(f32, f32)>,
    /// smooth mode: mean clock (position, velocity)
    ta: (f32, f32),
    hy: Vec<f32>,
    /// seconds since each shown number (per-CPU loads, clocks, then `hy`) changed
    na: Vec<f32>,
    scratch: std::cell::RefCell<Vec<f32>>,
    /// smooth mode: seconds since the newest history point (graphs scroll by it) and
    /// the animation frames drawn (history bodies redraw on a cycle of them, `body_key`)
    pub phase: f32,
    pub frame: u64,
    /// drawing an animation frame (the order is kept; it changes with samples)
    pub anim: bool,
    /// widest clock / load bar in cells (sets the animation's resolution)
    bar_cells: (usize, usize),
    /// memory, swap, disks, filesystems, processes
    pub m: crate::mem::Mem,
    /// page: 0 CPU (with the memory column beside it on big screens), 1 memory & storage
    pub page: usize,
    pub ml: MemLay,
    ma: MA,
    mcache: Vec<u64>,
    /// view 4: processes (procui.rs)
    pub pv: PV,
    /// colour of the series in `vals2` that `graph` draws under its own (0: none)
    pub under: std::cell::Cell<u32>,
}

impl App {
    pub fn new(s: Sampler, t: Theme) -> App {
        let n = s.t.groups.len();
        let maxid =
            s.t.cpus
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
                .max(n.saturating_sub(1));
        let idw = maxid.to_string().len().max(2);
        let rh_full =
            s.t.groups
                .iter()
                .map(|g| g.len())
                .max()
                .unwrap_or(1)
                .clamp(1, 4);
        App {
            s,
            t,
            sort: 0,
            view: 0,
            speed: 0,
            paused: false,
            show_top: true,
            show_det: true,
            det_core: false,
            selected: 0,
            order: (0..n).collect(),
            lay: Layout::default(),
            w: 0,
            cw: 0,
            h: 0,
            zones: Vec::new(),
            scroll: 0,
            clock: (0, 0, 0),
            uptime: 0,
            vals: Vec::with_capacity(2048),
            vals2: Vec::with_capacity(2048),
            tmp16: Vec::with_capacity(2048),
            tmp8: Vec::with_capacity(2048),
            idw,
            rh_full,
            menu: None,
            cache: vec![u64::MAX; SLOT_CARDS + n],
            prev_order: Vec::with_capacity(n),
            row_memo: Vec::new(),
            title_short: std::cell::Cell::new(false),
            mode_line: String::new(),
            depth_name: "truecolor",
            bf: Vec::new(),
            bu: Vec::new(),
            vf: Vec::new(),
            vu: Vec::new(),
            tf: Vec::new(),
            tu: Vec::new(),
            nf: Vec::new(),
            nu: Vec::new(),
            tt: Vec::new(),
            ta: (0.0, 0.0),
            hy: Vec::new(),
            na: Vec::new(),
            scratch: std::cell::RefCell::new(Vec::with_capacity(512)),
            phase: 0.0,
            frame: 0,
            anim: false,
            bar_cells: (8, 8),
            m: crate::mem::Mem::empty(),
            page: 0,
            ml: MemLay::default(),
            ma: MA::default(),
            mcache: Vec::new(),
            pv: PV::new(),
            under: std::cell::Cell::new(0),
        }
        .with_bars()
    }

    fn with_bars(mut self) -> App {
        let n = self.s.f.len();
        self.bf = vec![0.0; n];
        self.bu = vec![0.0; n];
        self.vf = vec![0.0; n];
        self.vu = vec![0.0; n];
        self.tf = vec![0.0; n];
        self.tu = vec![0.0; n];
        self.nf = vec![0.0; n];
        self.nu = vec![0.0; n];
        self.tt = self.s.temps.iter().map(|t| (t.val, 0.0)).collect();
        self.hy = vec![0.0; HY_TEMP + self.s.temps.len() + 2 * self.s.t.ndom.max(1)];
        self.bars_snap();
        self
    }

    pub fn smooth(&self) -> bool {
        self.speed == SMOOTH
    }

    /// Clock (MHz) and load a CPU's numbers show: the published sample, or in smooth
    /// mode the digits following its moving bars (`anim_numbers`).
    #[inline]
    pub fn shown(&self, c: usize) -> (f32, f32) {
        if self.smooth() {
            (self.nf[c], self.nu[c])
        } else {
            (self.s.f[c], self.s.u[c])
        }
    }

    /// Bars jump to the latest sample (every speed but smooth; smooth at start).
    pub fn bars_snap(&mut self) {
        let m = self.s.t.maxf;
        for i in 0..self.bf.len() {
            self.bf[i] = (self.s.fe[i] / m).clamp(0.0, 1.0);
            self.bu[i] = self.s.ur[i].clamp(0.0, 1.0);
            self.vf[i] = 0.0;
            self.vu[i] = 0.0;
        }
        self.tf.copy_from_slice(&self.bf);
        self.tu.copy_from_slice(&self.bu);
        for (t, s) in self.tt.iter_mut().zip(&self.s.temps) {
            *t = (s.val, 0.0);
        }
        self.ta = (self.s.avg_r, 0.0);
        self.hy.iter_mut().for_each(|h| *h = -1e9);
        self.na.clear();
        self.anim_numbers(1.0);
        self.mem_retarget(false);
    }

    /// Smooth mode, after a sample: the bars' new targets.
    /// Changes within the measurement noise (load: tick quantisation of the long
    /// window; clock: reading jitter) are ignored, so quiet bars stand still instead
    /// of shimmering at their edge; 0 and 100 % are always exact.
    pub fn retarget(&mut self) {
        let m = self.s.t.maxf;
        let db = |t: &mut f32, n: f32, d: f32| {
            if (n - *t).abs() >= d || n <= 0.0 || n >= 1.0 {
                *t = n;
            }
        };
        for i in 0..self.tf.len() {
            db(
                &mut self.tf[i],
                (self.s.fe[i] / m).clamp(0.0, 1.0),
                DEADBAND_F,
            );
            db(&mut self.tu[i], self.s.ur[i].clamp(0.0, 1.0), DEADBAND_U);
        }
        self.mem_retarget(true);
    }

    /// Smooth mode: advance every spring by `dt` s. Returns whether anything moves.
    pub fn anim_step(&mut self, dt: f32) -> bool {
        use crate::anim::*;
        let (eu, ef) = (
            0.25 / (self.bar_cells.1.max(1) * 16) as f32,
            0.25 / (self.bar_cells.0.max(1) * 16) as f32,
        );
        let (xu, xf, xt) = (
            (-W_LOAD * dt).exp(),
            (-W_CLOCK * dt).exp(),
            (-W_TEMP * dt).exp(),
        );
        let mut moving = false;
        for i in 0..self.bf.len() {
            moving |= spring(
                &mut self.bu[i],
                &mut self.vu[i],
                self.tu[i],
                dt,
                W_LOAD,
                xu,
                eu,
            );
            moving |= spring(
                &mut self.bf[i],
                &mut self.vf[i],
                self.tf[i],
                dt,
                W_CLOCK,
                xf,
                ef,
            );
        }
        for (t, s) in self.tt.iter_mut().zip(&self.s.temps) {
            moving |= spring(&mut t.0, &mut t.1, s.val, dt, W_TEMP, xt, 0.01);
        }
        let ta = &mut self.ta;
        moving |= spring(&mut ta.0, &mut ta.1, self.s.avg_r, dt, W_CLOCK, xf, 1.0);
        moving | self.mem_step(dt) | self.proc_step(dt)
    }

    /// Smooth mode: the digits follow the moving values (with hysteresis, see
    /// `anim::hyst`). Returns whether any changed.
    pub fn anim_numbers(&mut self, dt: f32) -> bool {
        use crate::anim::hyst;
        let m = self.s.t.maxf;
        let n = self.bf.len();
        if self.na.len() != 2 * n + self.hy.len() {
            self.na = vec![1.0; 2 * n + self.hy.len()];
        }
        // A digit field changes at most 4 times a second while its bar wanders, but
        // counts along at frame rate through a real transition (bar moving faster than
        // half its length a second) and lands exactly once the bar rests. While the
        // bar moves, a wide hysteresis keeps a value at a rounding edge from flickering.
        let (u, f, top) = self.live_totals();
        let nd = self.s.t.ndom.clamp(1, 16);
        let mut ccd = [(0.0f32, 0.0f32); 16];
        for (d, c) in ccd.iter_mut().enumerate().take(nd) {
            *c = self.ccd_live(d);
        }
        let mut ch = false;
        let na = &mut self.na;
        let mut upd = |k: usize, shown: &mut f32, x: f32, unit: f32, h: f32, v: Option<f32>| {
            na[k] += dt;
            let rest = v == Some(0.0);
            if !(rest || v.is_some_and(|v| v.abs() >= 0.5) || na[k] >= 0.25) {
                return;
            }
            if hyst(shown, x, unit, if rest { 0.0 } else { h }) {
                na[k] = 0.0;
                ch = true;
            }
        };
        for i in 0..n {
            let mut p = self.nu[i] * 100.0;
            upd(i, &mut p, self.bu[i] * 100.0, 1.0, 1.0, Some(self.vu[i]));
            self.nu[i] = p / 100.0;
            upd(
                i + n,
                &mut self.nf[i],
                self.bf[i] * m,
                10.0,
                2.0,
                Some(self.vf[i]),
            );
        }
        let hy = &mut self.hy;
        upd(2 * n + HY_TOT, &mut hy[HY_TOT], u * 100.0, 1.0, 0.5, None);
        upd(2 * n + HY_AVG, &mut hy[HY_AVG], f, 10.0, 1.0, None);
        upd(2 * n + HY_TOP, &mut hy[HY_TOP], top, 10.0, 1.0, None);
        let nt = self.tt.len();
        for k in 0..nt {
            let j = HY_TEMP + k;
            upd(2 * n + j, &mut hy[j], self.tt[k].0, 1.0, 0.3, None);
        }
        for (d, &(u, f)) in ccd.iter().enumerate().take(nd) {
            let b = HY_TEMP + nt + 2 * d;
            upd(2 * n + b, &mut hy[b], u * 100.0, 1.0, 0.5, None);
            upd(2 * n + b + 1, &mut hy[b + 1], f, 10.0, 1.0, None);
        }
        ch
    }

    /// Smooth mode: total load and top clock (MHz) from the moving bars, mean clock
    /// (of the raw readings, like its history) eased.
    fn live_totals(&self) -> (f32, f32, f32) {
        let n = self.bf.len().max(1) as f32;
        let m = self.s.t.maxf;
        let u = self.bu.iter().sum::<f32>() / n;
        let f = self.ta.0;
        let top = self.bf.iter().copied().fold(0.0, f32::max) * m;
        (u, f, top)
    }

    /// Smooth mode: a CCD's mean load and mean core clock from the moving bars.
    fn ccd_live(&self, d: usize) -> (f32, f32) {
        let s = &self.s;
        let (mut u, mut f, mut nt, mut nc) = (0.0f32, 0.0f32, 0usize, 0usize);
        for (g, thr) in s.t.groups.iter().enumerate() {
            if s.t.ccd[g] != d {
                continue;
            }
            nc += 1;
            f += thr.iter().map(|&c| self.bf[c]).fold(0.0, f32::max);
            for &c in thr {
                u += self.bu[c];
                nt += 1;
            }
        }
        (u / nt.max(1) as f32, f / nc.max(1) as f32 * s.t.maxf)
    }

    /// Total load (0..1), mean clock and top clock as the numbers show them.
    pub fn totals(&self) -> (f32, f32, f32) {
        if self.smooth() {
            (self.hy[HY_TOT] / 100.0, self.hy[HY_AVG], self.hy[HY_TOP])
        } else {
            let s = &self.s;
            (s.tot_u, s.avg_f, s.f.iter().copied().fold(0.0, f32::max))
        }
    }

    /// Sensor `k`: (bar position, shown value) in °C.
    pub fn temp(&self, k: usize) -> (f32, f32) {
        if self.smooth() {
            (self.tt[k].0, self.hy[HY_TEMP + k])
        } else {
            let v = self.s.temps[k].val;
            (v, v)
        }
    }

    /// CCD `d`: (bar load, shown load, shown clock).
    pub fn ccd(&self, d: usize) -> (f32, f32, f32) {
        if self.smooth() {
            let b = HY_TEMP + self.tt.len() + 2 * d;
            (self.ccd_live(d).0, self.hy[b] / 100.0, self.hy[b + 1])
        } else {
            let (u, f, _) = ccd_stats(&self.s, d);
            (u, u, f)
        }
    }

    /// Temperature shown for an L3 domain.
    pub fn dom_temp(&self, d: usize) -> Option<f32> {
        self.s.domain_temp_idx(d).map(|k| self.temp(k).1)
    }

    /// Redraw key of history bodies (graphs, strips, heatmap). Smooth: changes every
    /// GRAPH_DIV frames, at frame `slot` of the cycle (0: the two top graphs, 2: the
    /// detail area and the sensor history; strips: by core number at half the rate),
    /// so frames cost about the same.
    pub fn body_key(&self, slot: u64) -> u64 {
        if self.smooth() {
            (self.frame + crate::anim::GRAPH_DIV - slot) / crate::anim::GRAPH_DIV
        } else {
            self.s.hist_n / self.hist_step()
        }
    }

    /// Redraw key of core `g`'s history strip (smooth: staggered over the cores).
    pub fn strip_key(&self, g: usize) -> u64 {
        if self.smooth() {
            (self.frame + g as u64) / (2 * crate::anim::GRAPH_DIV)
        } else {
            self.body_key(0)
        }
    }

    /// `n` values of a history series for columns `step` seconds apart, the youngest
    /// at age `off` (oldest first). Other speeds: the last `n` points as they are
    /// (`step` 1); smooth: the scrolling curve (`anim::Curve`) with the live value at
    /// its head.
    pub fn hist_vals(&self, out: &mut Vec<f32>, n: usize, off: f32, step: f32, src: Hist) {
        let s = &self.s;
        let (len, head, part): (usize, f32, f32) = match src {
            Hist::Tot => (s.h_tot.len(), self.live_totals().0, s.part_tot()),
            Hist::Avg => {
                let p =
                    (0..s.fr.len()).map(|i| s.part_f(i)).sum::<f32>() / s.fr.len().max(1) as f32;
                (s.h_avg.len(), self.ta.0, p)
            }
            Hist::Temp(k) => (s.temps[k].hist.len(), self.tt[k].0, s.temps[k].val),
            Hist::Load(t) => {
                let k = t.len().max(1) as f32;
                (
                    t.iter().map(|&c| s.hu[c].len()).min().unwrap_or(0),
                    t.iter().map(|&c| self.bu[c]).sum::<f32>() / k,
                    t.iter().map(|&c| s.part_u(c)).sum::<f32>() / k,
                )
            }
            // (history points are means of the raw readings, idle floor included: so
            // is the head, not the bars' clock-while-running)
            Hist::Clock(t) => {
                let p = t.iter().map(|&c| s.part_f(c)).fold(0.0, f32::max);
                (t.iter().map(|&c| s.hf[c].len()).min().unwrap_or(0), p, p)
            }
        };
        let pt = |k: usize| -> f32 {
            match src {
                Hist::Tot => s.h_tot.back(k),
                Hist::Avg => s.h_avg.back(k),
                Hist::Temp(i) => s.temps[i].hist.back(k),
                Hist::Load(t) => {
                    t.iter().map(|&c| s.hu[c].back(k) as f32).sum::<f32>()
                        / (255.0 * t.len().max(1) as f32)
                }
                Hist::Clock(t) => t
                    .iter()
                    .map(|&c| s.hf[c].back(k) as f32)
                    .fold(0.0, f32::max),
            }
        };
        if !self.smooth() {
            out.clear();
            let have = len.min(n);
            out.resize(n - have, 0.0);
            out.extend((0..have).rev().map(pt));
            return;
        }
        crate::anim::Curve {
            phase: self.phase,
            head,
            part,
            len,
            pt: &pt,
            pad: 0.0,
        }
        .sample(out, n, off, step);
    }

    /// History points between redraws of graphs and strips: 2 (one braille column),
    /// more on very large screens, where redrawing ~20k graph cells is the bulk of a
    /// frame's cost; the frames in between then update only bars and numbers, so the
    /// eco budget affords more of them.
    pub fn hist_step(&self) -> u64 {
        match self.w * self.h {
            0..12_000 => 2,
            12_000..20_000 => 4,
            _ => 6,
        }
    }

    pub fn ngroups(&self) -> usize {
        self.s.t.groups.len()
    }

    // ── layout ─────────────────────────────────────────────────────────────

    /// Choose the card mode, columns and which panels fit (hero first: all cores
    /// visible, then the graphs, then the detail panel; breathing room last).
    fn plan(&self, w: usize, h: usize) -> Option<Plan> {
        let n = self.ngroups();
        let avail = h.saturating_sub(2);
        let iw = w.saturating_sub(4);
        let ndom = self.s.t.ndom.max(1);
        let modes: &[u8] = match self.view {
            1 => &[M_FULL],
            2 => &[M_COMPACT, M_MICRO],
            _ => &[M_FULL, M_COMPACT, M_MICRO],
        };
        for &m in modes {
            let rh = if m == M_FULL { self.rh_full } else { 1 };
            let (mw, g) = (MINW[m as usize], GUT[m as usize]);
            let mut best: Option<(usize, Plan)> = None;
            for cols in 1..=n.max(1) {
                if cols * (mw + g) > iw + g {
                    break;
                }
                let nrows = n.div_ceil(cols);
                let core = nrows * rh + 2;
                if core > avail {
                    continue;
                }
                let rest = avail - core;
                let header = rest >= 1;
                let rest = rest - header as usize;
                let top = self.show_top && rest >= TOP_MINI;
                let det = self.show_det && rest >= if top { TOP_MIN } else { 0 } + DET_MIN;
                let score = (top as usize) * 64
                    + (cols == ndom && n % cols == 0) as usize * 32
                    + (det as usize) * 16
                    + (n % cols == 0) as usize * 8
                    + (8usize.saturating_sub(cols));
                if best.as_ref().is_none_or(|b| score > b.0) {
                    best = Some((
                        score,
                        Plan {
                            mode: m,
                            cols,
                            nrows,
                            rh,
                            header,
                            top,
                            det,
                            rest,
                        },
                    ));
                }
            }
            if let Some((_, p)) = best {
                return Some(p);
            }
        }
        None
    }

    pub fn first_size(&mut self, w: usize, h: usize) {
        self.show_top = true;
        self.show_det = true;
        self.fit(w, h);
    }

    /// Lay out the page for a `w` × `h` screen: the CPU view (with the memory column
    /// beside it from SIDE_W × SIDE_H up, as long as every core still fits) or the
    /// memory page.
    pub fn fit(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        let area = Rect {
            x: 0,
            y: 1,
            w,
            h: h.saturating_sub(2),
        };
        self.ml = MemLay::default();
        if self.page == 1 {
            self.fit_cpu(w, h);
            self.ml = self.mem_layout(area, w >= 100 && area.h >= 20);
            return;
        }
        // views 3 (network, placeholder) and 4 (processes, procui.rs)
        self.pv.lay = PLay::default();
        if self.page == PG_NET || self.page == PG_PROC {
            self.fit_cpu(w, h);
            if self.page == PG_PROC {
                self.proc_layout(area);
                if self.pv.d.is_some() {
                    self.psync(true);
                }
            }
            return;
        }
        if w >= SIDE_W && h >= SIDE_H {
            let sw = (w * 26 / 100).clamp(56, 100);
            self.fit_cpu(w - sw, h);
            if self.lay.fits && self.lay.nrows <= self.lay.vis_rows {
                self.ml = self.mem_layout(
                    Rect {
                        x: w - sw,
                        w: sw,
                        ..area
                    },
                    false,
                );
                return;
            }
        }
        self.fit_cpu(w, h);
    }

    /// CPU view layout in the leftmost `w` columns.
    fn fit_cpu(&mut self, w: usize, h: usize) {
        self.cw = w;
        let n = self.ngroups();
        let avail = h.saturating_sub(2);
        let plan = self.plan(w, h);
        let mut l = Layout {
            fits: plan.is_some(),
            ..Default::default()
        };
        let (used, mut th, dh);
        if let Some(p) = plan {
            l.mode = p.mode;
            l.compact = p.mode != M_FULL;
            l.cols = p.cols;
            l.nrows = p.nrows;
            l.rh = p.rh;
            l.header = p.header;
            let mut r = p.rest
                - match (p.top, p.det) {
                    (true, true) => TOP_MIN + DET_MIN,
                    (true, false) => TOP_MIN.min(p.rest),
                    (false, true) => DET_MIN,
                    _ => 0,
                };
            // breathing room between cores once the panels have grown a little
            let gaps = p.nrows.saturating_sub(1);
            l.gap = p.mode != M_MICRO && gaps > 0 && r >= gaps + 6;
            if l.gap {
                r -= gaps;
            }
            // CCD divider rows: free in the gap row, else one spare row
            let mixed = self.sort == 0 && (0..p.cols).any(|c| self.split_in(c, p.nrows).is_some());
            if mixed && p.mode != M_MICRO && (l.gap || r >= 1) {
                l.div = true;
                if !l.gap {
                    r -= 1;
                }
            }
            let core = p.nrows * p.rh
                + 2
                + p.header as usize
                + if l.gap { gaps } else { 0 }
                + (l.div && !l.gap) as usize;
            match (p.top, p.det) {
                (true, true) => {
                    // the graph band stops growing on tall screens; the rest goes to the
                    // detail area (which becomes the per-core history grid)
                    th = (TOP_MIN + (r * 55 + 50) / 100).min(10 + h / 10);
                    // the all-core heatmap wants a row per two cores (+ borders): borrow
                    // a row or two from the graph band when that makes it fit
                    let want = n.div_ceil(2) + 2;
                    let short = want.saturating_sub(avail - core - th);
                    if !self.det_core && short > 0 && short <= 2 && th >= TOP_MIN + 1 + short {
                        th -= short;
                    }
                    dh = avail - core - th;
                }
                (true, false) => {
                    th = avail - core;
                    dh = 0;
                }
                (false, true) => {
                    th = 0;
                    dh = avail - core;
                }
                _ => {
                    th = 0;
                    dh = 0;
                }
            }
            used = avail - th - dh;
        } else {
            // nothing fits: densest layout, scroll the rest
            l.mode = M_MICRO;
            l.compact = true;
            let iw = w.saturating_sub(4);
            l.cols = ((iw + GUT[2]) / (MINW[2] + GUT[2])).clamp(1, n.max(1));
            l.nrows = n.div_ceil(l.cols);
            l.rh = 1;
            l.header = false;
            th = 0;
            dh = 0;
            used = avail;
        }
        let mut y = 1;
        if th > 0 {
            let top = Rect { x: 0, y, w, h: th };
            let sw = if w >= 120 {
                46
            } else {
                (w * 36 / 100).max(24).min(w)
            };
            let gw = w - sw;
            let gl = gw / 2;
            l.g_load = Rect {
                x: 0,
                y,
                w: gl,
                h: th,
            };
            l.g_freq = Rect {
                x: gl,
                y,
                w: gw - gl,
                h: th,
            };
            l.sensors = Rect {
                x: gw,
                y,
                w: sw,
                h: th,
            };
            l.top = Some(top);
            y += th;
        }
        l.cores = Rect {
            x: 0,
            y,
            w,
            h: used,
        };
        y += used;
        if dh > 0 {
            l.det = Some(Rect { x: 0, y, w, h: dh });
            l.grid = !self.det_core
                && dh >= 18
                && w >= 150
                && n >= 4
                && self.grid_geo(l.det.unwrap()).is_some();
        }
        let inner = l.cores.inner();
        let cols = l.cols.max(1);
        let g = GUT[l.mode as usize];
        let total = inner.w.saturating_sub(g * (cols - 1));
        let (base, extra) = (total / cols, total % cols);
        let mut x = 0;
        // equal columns: the leftover cells go to the gutters' right side
        for c in 0..cols {
            l.card_x.push((x, base));
            x += base + g + (c < extra) as usize;
        }
        let step = l.rh + l.gap as usize;
        l.vis_rows = (inner.h.saturating_sub(l.header as usize) + l.gap as usize) / step.max(1);
        self.bar_cells = l.card_x.iter().fold((1, 1), |a, &(_, cw)| {
            let geo = self.geo(cw, l.mode);
            (a.0.max(geo.fw), a.1.max(geo.lw))
        });
        self.lay = l;
        self.fix_scroll();
    }

    fn fix_scroll(&mut self) {
        let l = &self.lay;
        if l.nrows <= l.vis_rows || l.nrows == 0 {
            self.scroll = 0;
            return;
        }
        let k = self
            .order
            .iter()
            .position(|&g| g == self.selected)
            .unwrap_or(0);
        let r = k % l.nrows;
        if r < self.scroll {
            self.scroll = r;
        } else if r >= self.scroll + l.vis_rows {
            self.scroll = r + 1 - l.vis_rows;
        }
        self.scroll = self.scroll.min(l.nrows - l.vis_rows);
    }

    pub fn resort(&mut self) {
        let s = &self.s;
        let n = self.ngroups();
        self.order.clear();
        self.order.extend(0..n);
        if self.sort == 0 {
            return;
        }
        let v = if self.sort == 1 { &s.u } else { &s.f };
        let key: Vec<f32> =
            s.t.groups
                .iter()
                .map(|g| g.iter().map(|&c| v[c]).fold(0.0, f32::max))
                .collect();
        crate::sys::isort(&mut self.order, &|&a, &b| key[a] > key[b]);
    }

    // ── actions ────────────────────────────────────────────────────────────

    pub fn move_sel(&mut self, dx: isize, dy: isize) {
        let nr = self.lay.nrows.max(1);
        let k = self
            .order
            .iter()
            .position(|&g| g == self.selected)
            .unwrap_or(0);
        let (c, r) = ((k / nr) as isize + dx, (k % nr) as isize + dy);
        let j = c * nr as isize + r;
        if c >= 0
            && (c as usize) < self.lay.cols
            && r >= 0
            && (r as usize) < nr
            && (j as usize) < self.order.len()
        {
            self.selected = self.order[j as usize];
            self.fix_scroll();
        }
    }

    /// Which group's card is at screen cell (x, y)?
    /// First row (within column `c`, `nrows` rows a column) of a second CCD, if any.
    fn split_in(&self, c: usize, nrows: usize) -> Option<usize> {
        // only in core order (identity), where a CCD is a contiguous run
        if self.sort != 0 {
            return None;
        }
        let d = |r: usize| self.s.t.ccd.get(c * nrows + r).copied();
        (1..nrows).find(|&r| d(r).is_some() && d(r) != d(r - 1))
    }

    /// Screen row of visible row `vr` in column `c` (divider rows push later rows down).
    fn row_y(&self, c: usize, vr: usize) -> usize {
        let l = &self.lay;
        let y0 = l.cores.inner().y + l.header as usize;
        let step = l.rh + l.gap as usize;
        let past = l.div
            && !l.gap
            && self
                .split_in(c, l.nrows)
                .is_some_and(|s| vr + self.scroll >= s);
        y0 + vr * step + past as usize
    }

    pub fn card_at(&self, x: usize, y: usize) -> Option<usize> {
        if self.page != 0 {
            return None;
        }
        if let Some(g) = self.grid_at(x, y) {
            return Some(g);
        }
        let l = &self.lay;
        let inner = l.cores.inner();
        if !inner.contains(x, y) {
            return None;
        }
        for (c, &(cx, cw)) in l.card_x.iter().enumerate() {
            if x >= inner.x + cx && x < inner.x + cx + cw {
                for vr in 0..l.vis_rows.min(l.nrows.saturating_sub(self.scroll)) {
                    let ry = self.row_y(c, vr);
                    if y >= ry && y < ry + l.rh {
                        return self.order.get(c * l.nrows + vr + self.scroll).copied();
                    }
                }
            }
        }
        None
    }

    pub fn key_at(&self, x: usize, y: usize) -> Option<char> {
        if y + 1 != self.h {
            return None;
        }
        self.zones.iter().find(|z| x >= z.0 && x < z.1).map(|z| z.2)
    }

    // ── drawing ────────────────────────────────────────────────────────────
    //
    // Retained mode: `full` frames (input, resize, theme) redraw everything; live
    // frames (ticks) redraw only elements whose memo key changed, and write every row
    // of an element exactly once (padding instead of clearing) so unchanged cells stay
    // unchanged and cost nothing in the flush.

    fn memo(&mut self, slot: usize, key: u64) -> bool {
        if self.cache.len() <= slot {
            self.cache.resize(slot + 1, u64::MAX);
        }
        if self.cache[slot] == key {
            return false;
        }
        self.cache[slot] = key;
        true
    }

    /// Smooth-mode in-between frame: only the core cards (their bars and digits);
    /// the panels and graphs change on the other frames, in `draw`.
    pub fn draw_bars(&mut self, scr: &mut Screen) {
        if self.menu.is_none() {
            if self.page == 0 {
                prof(11, || self.draw_cores(scr, false));
            }
            if self.mem_visible() {
                self.draw_mem(scr, false, false);
            }
            if self.page == PG_PROC {
                self.draw_proc_page(scr, false, true);
            }
        }
    }

    pub fn draw(&mut self, scr: &mut Screen, full: bool) {
        let full = full || self.menu.is_some();
        if !self.paused && !self.anim {
            prof(13, || {
                self.prev_order.clone_from(&self.order);
                self.resort();
            });
        }
        if full {
            self.cache.iter_mut().for_each(|c| *c = u64::MAX);
            self.row_memo.clear();
            scr.clear(BLANK);
        } else if self.order != self.prev_order {
            for c in self.cache.iter_mut().skip(SLOT_CARDS) {
                *c = u64::MAX;
            }
        }
        let hn = self.s.hist_n;
        let (h, m, sec) = self.clock;
        let sec = if self.speed > 0 { sec } else { 0 };
        // without the graph band the title carries a live summary (load, clock, temp)
        let cpu = self.page == 0;
        let summary = if cpu && self.lay.top.is_none() {
            let (u, f, t) = self.summary();
            mixk(&[
                (u * 100.0).round() as u64,
                (f / 10.0) as u64,
                t.round() as u64,
            ])
        } else {
            0
        };
        if self.memo(
            SLOT_TITLE,
            mixk(&[
                (h * 3600 + m * 60 + sec) as u64,
                self.uptime / 60,
                self.paused as u64,
                self.speed as u64,
                summary,
            ]),
        ) {
            let full = full || !cpu || self.lay.top.is_none();
            prof(8, || self.draw_title(scr, full));
        }
        if cpu && self.lay.top.is_some() {
            let (u, f, topf) = self.totals();
            let ku = (u * 100.0).round() as u64;
            let kf = mixk(&[(f / 10.0) as u64, (topf / 10.0) as u64]);
            let ks = mixk(&[sensors_key(&self.s), hn / 2]);
            let kd = self.sensors_dyn_key();
            let kh = self.body_key(2);
            let body = self.memo(SLOT_GBODY, self.body_key(0));
            if self.memo(SLOT_GLOAD, ku) || body {
                prof(9, || self.draw_gload(scr, full, body));
            }
            let body = self.memo(SLOT_GBODY2, self.body_key(0));
            if self.memo(SLOT_GFREQ, kf) || body {
                prof(9, || self.draw_gfreq(scr, full, body));
            }
            // static rows, live rows (temperatures, CCDs), temperature history
            let parts = self.memo(SLOT_SENSORS, ks) as u8
                | (self.memo(SLOT_SDYN, kd) as u8) << 1
                | (self.memo(SLOT_SHIST, kh) as u8) << 2;
            if parts != 0 || full {
                prof(10, || {
                    self.draw_sensors(scr, full, if full { 7 } else { parts })
                });
            }
        }
        if cpu {
            prof(11, || self.draw_cores(scr, full));
        }
        if cpu && self.lay.det.is_some() {
            let g = self.selected;
            let s = &self.s;
            let mut k = g as u64;
            let all = if self.lay.grid { s.f.len() } else { 0 };
            for c in s.t.groups[g].iter().copied().chain(0..all) {
                let (f, u) = self.shown(c);
                k = mixk(&[
                    k,
                    (f / 10.0) as u64,
                    (u * 100.0).round() as u64,
                    (s.peak[c] / 10.0) as u64,
                ]);
            }
            let body = self.memo(SLOT_DBODY, self.body_key(2));
            if self.memo(SLOT_DETAIL, k) || body {
                prof(12, || self.draw_detail(scr, full, body));
            }
        }
        if self.mem_visible() {
            prof(15, || self.draw_mem(scr, full, true));
        }
        if self.page == PG_PROC {
            prof(15, || self.draw_proc_page(scr, full, false));
        } else if self.page == PG_NET {
            self.draw_net_placeholder(scr, full);
        }
        if full {
            self.draw_keys(scr);
            if self.menu.is_some() {
                self.draw_menu(scr);
            }
        }
    }

    pub fn spans(
        scr: &mut Screen,
        mut x: usize,
        y: usize,
        maxx: usize,
        bg: u32,
        sp: &[(&str, u32)],
    ) -> usize {
        for (s, fg) in sp {
            x = scr.text(x, y, s, *fg, bg, maxx);
        }
        x
    }

    /// Spans, then blanks up to `maxx` (a row written once, no clearing).
    pub fn line(scr: &mut Screen, x: usize, y: usize, maxx: usize, bg: u32, sp: &[(&str, u32)]) {
        let mut x = Self::spans(scr, x, y, maxx, bg, sp);
        while x < maxx {
            scr.put(x, y, ' ', 0, bg);
            x += 1;
        }
    }

    fn spans_len(sp: &[(&str, u32)]) -> usize {
        sp.iter().map(|(s, _)| s.chars().count()).sum()
    }

    /// The first title variant that fits a panel `w` cells wide (else the last).
    fn pick<'a>(w: usize, vs: &'a [&'a [(&'a str, u32)]]) -> &'a [(&'a str, u32)] {
        vs.iter()
            .find(|v| Self::spans_len(v) + 4 <= w)
            .unwrap_or(&vs[vs.len() - 1])
    }

    /// Top border row with a title (rewritten as a whole).
    fn panel_top(&self, scr: &mut Screen, r: Rect, title: &[(&str, u32)]) {
        let bg = surf(S_PANEL);
        let b = self.t.border;
        let x1 = r.x + r.w - 1;
        scr.put(r.x, r.y, '╭', b, bg);
        let mut x = r.x + 1;
        if !title.is_empty() && r.w > 6 {
            scr.put(x, r.y, ' ', b, bg);
            x = Self::spans(scr, x + 1, r.y, x1 - 1, bg, title);
            scr.put(x, r.y, ' ', b, bg);
            x += 1;
        }
        while x < x1 {
            scr.put(x, r.y, '─', b, bg);
            x += 1;
        }
        scr.put(x1, r.y, '╮', b, bg);
    }

    pub fn panel(&self, scr: &mut Screen, r: Rect, title: &[(&str, u32)], sub: &str) {
        if r.w < 2 || r.h < 2 {
            return;
        }
        let t = &self.t;
        let bg = surf(S_PANEL);
        let b = t.border;
        scr.fill(
            r.x,
            r.y,
            r.w,
            r.h,
            Cell {
                ch: ' ',
                fg: STAR_OK,
                bg,
            },
        );
        let (x1, y1) = (r.x + r.w - 1, r.y + r.h - 1);
        for x in r.x + 1..x1 {
            scr.put(x, y1, '─', b, bg);
        }
        for y in r.y + 1..y1 {
            scr.put(r.x, y, '│', b, bg);
            scr.put(x1, y, '│', b, bg);
        }
        self.panel_top(scr, r, title);
        scr.put(r.x, y1, '╰', b, bg);
        scr.put(x1, y1, '╯', b, bg);
        let sl = sub.chars().count();
        if sl > 0 && sl + 6 < r.w {
            let x = x1 - 2 - sl;
            scr.put(x - 1, y1, ' ', b, bg);
            scr.text(x, y1, sub, t.dim, bg, x1 - 1);
            scr.put(x1 - 2, y1, ' ', b, bg);
        }
    }

    /// Total load, fastest clock (MHz) and hottest sensor (°C, 0 if none).
    fn summary(&self) -> (f32, f32, f32) {
        let (u, _, f) = self.totals();
        let t = (0..self.s.temps.len())
            .map(|k| self.temp(k).1)
            .fold(0.0, f32::max);
        (u, f, t)
    }

    /// Memo key of the sensors panel's live rows (smooth: as they move).
    fn sensors_dyn_key(&self) -> u64 {
        let s = &self.s;
        let bw = self.sensors_bw() as f32 * 16.0;
        let mut k = 0;
        for i in 0..s.temps.len() {
            let (p, v) = self.temp(i);
            k = mixk(&[k, ((p - 30.0) / 65.0 * bw) as u64, v.to_bits() as u64]);
        }
        for d in 0..s.t.ndom {
            let (b, u, f) = self.ccd(d);
            k = mixk(&[k, (b * bw) as u64, u.to_bits() as u64, f.to_bits() as u64]);
        }
        k
    }

    /// Width of the sensors panel's meters.
    fn sensors_bw(&self) -> usize {
        let w = self.lay.sensors.inner().w;
        let tsw = if w >= 32 { 8 } else { 0 };
        w.saturating_sub(13 + if tsw > 0 { tsw + 2 } else { 0 })
            .max(4)
    }

    fn draw_title(&self, scr: &mut Screen, full: bool) {
        let t = &self.t;
        let w = self.w;
        let bar = surf(S_BAR);
        let s = &self.s.t;
        let (h, m, sec) = self.clock;
        let mut clock = F::new();
        // eco samples every 1-2 s, so seconds would tick unevenly: show them only
        // at a fixed speed
        let secs = [h, m, sec];
        for v in &secs[..if self.speed > 0 { 3 } else { 2 }] {
            let v = *v;
            clock.push(b'0' + (v / 10) as u8);
            clock.push(b'0' + (v % 10) as u8);
            clock.push(b':');
        }
        clock.n -= 1;
        clock.push(b' ');
        let up = {
            let u = self.uptime;
            let (d, hh, mm) = (u / 86400, u % 86400 / 3600, u % 3600 / 60);
            let mut f = F::new();
            if d > 0 {
                f.int(d as u32, 0);
                f.str("d ");
            }
            f.int(hh as u32, 0);
            f.str("h ");
            f.push(b'0' + (mm / 10) as u8);
            f.push(b'0' + (mm % 10) as u8);
            f.push(b'm');
            f
        };
        let tail: [(&str, u32); 4] = [
            ("   up ", t.dim),
            (up.s(), t.text),
            ("   ", 0),
            (clock.s(), t.fg | BOLD),
        ];
        let tail_short: [(&str, u32); 2] = [("   ", 0), (clock.s(), t.fg | BOLD)];
        if !full {
            // live frame: only the clock / uptime part changes
            let tl: &[(&str, u32)] = if self.title_short.get() {
                &tail_short
            } else {
                &tail
            };
            let tlen = Self::spans_len(tl);
            Self::spans(scr, w.saturating_sub(tlen), 0, w, bar, tl);
            return;
        }
        let topo = f!("   {} cores · {} threads", s.groups.len(), s.cpus.len());
        let drv = f!(" · {}", s.driver);
        let short = f!("  {}", s.short_model);
        let brand = (" ◆ cpuview ", t.bg | BOLD);
        let spd = f!(" {}", SPEEDS[self.speed].0);
        let state_live: [(&str, u32); 3] = [("● ", t.good), ("live", t.mid), (spd.s(), t.dim)];
        let drv_sp = if s.driver.is_empty() {
            ("", 0)
        } else {
            (drv.s(), t.dim)
        };
        let (su, sf, st) = self.summary();
        let (pu, pf, pt) = (
            num(" ", su * 100.0, 0, "%"),
            ghz(sf),
            num("  ", st, 0, "°C"),
        );
        let maxf = self.s.t.maxf;
        let (cu, cf, ct) = (
            t.load.at(su) | BOLD,
            t.freq.at(sf / maxf),
            t.temp.at((st - 30.0) / 65.0),
        );
        let has_t = !self.s.temps.is_empty();
        let sum_full: [(&str, u32); 7] = [
            ("  cpu", t.dim),
            (pu.s(), cu),
            ("  ", 0),
            (pf.s(), cf),
            (" GHz", t.dim),
            (if has_t { pt.s() } else { "" }, ct),
            ("", 0),
        ];
        let sum_short: [(&str, u32); 2] =
            [(pu.s(), cu), (if has_t { &pt.s()[1..] } else { "" }, ct)];
        // (pages 2 to 4 have a title like the memory page's)
        let mem_page = self.page != 0;
        let no_top = !mem_page && self.lay.top.is_none();
        let mi = self.m.mi;
        let minfo = f!(
            "   {} RAM · {} swap",
            bsize(mi.total as f32, false).s(),
            bsize(mi.swap_total as f32, false).s()
        );
        let ndk = self.m.disks.len();
        let dinfo = f!(" · {} disk{}", ndk, if ndk == 1 { "" } else { "s" });
        let mtitle = (
            match self.page {
                PG_PROC => "  processes",
                PG_NET => "  network & system",
                _ => "  memory & storage",
            },
            t.fg | BOLD,
        );
        let mut left0 = [(short.s(), t.fg | BOLD); 8];
        left0[1..].copy_from_slice(&sum_full);
        // (left part, short tail = clock only without uptime), most detailed first
        let lefts: [(&[(&str, u32)], bool); 6] = if mem_page {
            [
                (&[mtitle, (minfo.s(), t.mid), (dinfo.s(), t.dim)], false),
                (&[mtitle, (minfo.s(), t.mid)], false),
                (&[mtitle], false),
                (&[mtitle], true),
                (&[], true),
                (&[], true),
            ]
        } else if no_top {
            [
                (&left0, false),
                (&sum_full, false),
                (&sum_full, true),
                (&sum_short, false),
                (&sum_short, true),
                (&[], true),
            ]
        } else {
            [
                (
                    &[(short.s(), t.fg | BOLD), (topo.s(), t.mid), drv_sp],
                    false,
                ),
                (&[(short.s(), t.fg | BOLD), (topo.s(), t.mid)], false),
                (&[(short.s(), t.fg | BOLD)], false),
                (&[(short.s(), t.fg | BOLD)], true),
                (&[], true),
                (&[], true),
            ]
        };
        let state_len = if self.paused {
            8
        } else {
            Self::spans_len(&state_live)
        };
        for (i, &(left, short_tail)) in lefts.iter().enumerate() {
            let tl: &[(&str, u32)] = if short_tail { &tail_short } else { &tail };
            let ll = 11 + Self::spans_len(left);
            let rl = state_len + Self::spans_len(tl);
            if ll + rl + 2 <= w || i == lefts.len() - 1 {
                self.title_short.set(short_tail);
                let mut x = scr.text(0, 0, brand.0, brand.1, t.accent, w);
                x = Self::spans(scr, x, 0, w, bar, left);
                let rx0 = w.saturating_sub(rl);
                while x < rx0 {
                    scr.put(x, 0, ' ', 0, bar);
                    x += 1;
                }
                let rx = if self.paused {
                    scr.text(rx0, 0, " PAUSED ", t.bg | BOLD, t.crit, w)
                } else {
                    Self::spans(scr, rx0, 0, w, bar, &state_live)
                };
                Self::line(scr, rx, 0, w, bar, tl);
                break;
            }
        }
    }

    /// Braille area graph of the most recent `2 * w` values in `vals` (chronological).
    #[allow(clippy::too_many_arguments)]
    pub fn graph(
        &self,
        scr: &mut Screen,
        x0: usize,
        y0: usize,
        w: usize,
        h: usize,
        vals: &[f32],
        vmax: f32,
        ramp: &crate::color::Ramp,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let bg = surf(S_PANEL);
        let top = (h * 4) as f32;
        // dot height of every half-column, computed once (not once per row)
        let w = w.min(1024);
        // a second series drawn under the first in its own colour (`under`, `vals2`)
        let uc = self.under.get();
        let mut dots = [[0u16; 2]; 2048];
        for (k, vals) in [vals, &self.vals2[..]]
            .into_iter()
            .enumerate()
            .take(1 + (uc != 0) as usize)
        {
            let n = vals.len();
            for (i, d) in dots.iter_mut().enumerate().take(2 * w) {
                // i over 0..2w; vals are right-aligned (newest last)
                let j = n as isize - 2 * w as isize + i as isize;
                if j < 0 {
                    continue;
                }
                let v = vals[j as usize];
                if v > 0.0 {
                    d[k] = (v / vmax * top).round().max(1.0).min(top) as u16;
                }
            }
        }
        let dot = |i: usize| dots[i][0].max(dots[i][1]) as usize;
        let gl = [
            (0.25 * top).round() as usize,
            (0.5 * top).round() as usize,
            (0.75 * top).round() as usize,
        ];
        for r in 0..h {
            let base = (h - 1 - r) * 4;
            let col = ramp.at((h as f32 - r as f32 - 0.5) / h as f32);
            let grid_bit = (0..4).find(|k| gl.contains(&(base + k + 1)));
            for i in 0..w {
                let (a, b) = (dot(2 * i), dot(2 * i + 1));
                let mut code = 0;
                if a >= base + 4 && b >= base + 4 {
                    code = 0xff;
                } else if a > base || b > base {
                    for (k, &(lb, rb)) in BRAILLE.iter().enumerate() {
                        if a > base + k {
                            code |= lb;
                        }
                        if b > base + k {
                            code |= rb;
                        }
                    }
                }
                if code != 0 {
                    let mine = dots[2 * i][0] as usize > base || dots[2 * i + 1][0] as usize > base;
                    scr.put(
                        x0 + i,
                        y0 + r,
                        char::from_u32(0x2800 + code).unwrap(),
                        if mine { col } else { uc },
                        bg,
                    );
                } else if let (Some(k), true) = (grid_bit, i % 2 == 0) {
                    scr.put(
                        x0 + i,
                        y0 + r,
                        char::from_u32(0x2800 + BRAILLE[k].0).unwrap(),
                        self.t.border,
                        bg,
                    );
                } else {
                    scr.put(x0 + i, y0 + r, ' ', NOPATCH, bg);
                }
            }
        }
    }

    /// Graph with a y-axis label gutter.
    #[allow(clippy::too_many_arguments)]
    pub fn axis_graph(
        &self,
        scr: &mut Screen,
        x0: usize,
        y0: usize,
        w: usize,
        h: usize,
        vals: &[f32],
        vmax: f32,
        ramp: &crate::color::Ramp,
        labels: &[(f32, &str)],
    ) {
        let labels = if h < 3 { &labels[..0] } else { labels };
        let gut = labels
            .iter()
            .map(|l| l.1.chars().count())
            .max()
            .unwrap_or(0)
            + 1;
        if w < gut + 8 {
            self.graph(scr, x0, y0, w, h, vals, vmax, ramp);
            return;
        }
        self.graph(scr, x0 + gut, y0, w - gut, h, vals, vmax, ramp);
        let mut used = [false; 64];
        for r in 0..h.min(64) {
            for x in x0..x0 + gut {
                scr.put(x, y0 + r, ' ', 0, surf(S_PANEL));
            }
        }
        for &(frac, s) in labels {
            let r = (((1.0 - frac) * (h as f32 - 1.0)).round() as usize).min(h - 1);
            if r < 64 && !used[r] {
                used[r] = true;
                let l = s.chars().count();
                scr.text(
                    x0 + gut - 1 - l,
                    y0 + r,
                    s,
                    self.t.dim,
                    surf(S_PANEL),
                    x0 + gut,
                );
            }
        }
    }

    fn draw_gload(&mut self, scr: &mut Screen, full: bool, body: bool) {
        let r = self.lay.g_load;
        let th = &self.t;
        let u = self.totals().0;
        let ir = r.inner();
        let pct = num("", u * 100.0, 0, "%");
        let title: [(&str, u32); 2] = [("cpu ", th.title | BOLD), (pct.s(), th.load.at(u) | BOLD)];
        if full {
            let win = span(2 * ir.w.saturating_sub(5));
            let sub = f!("total load · last {}", win.s());
            self.panel(scr, r, &title, sub.s());
        } else {
            self.panel_top(scr, r, &title);
        }
        if !(full || body) {
            return;
        }
        let mut vals = std::mem::take(&mut self.vals);
        self.hist_vals(&mut vals, 2 * ir.w, 0.0, 1.0, Hist::Tot);
        self.axis_graph(
            scr,
            ir.x,
            ir.y,
            ir.w,
            ir.h,
            &vals,
            1.0,
            &self.t.load,
            &[(1.0, "100%"), (0.5, "50"), (0.0, "0")],
        );
        self.vals = vals;
    }

    fn draw_gfreq(&mut self, scr: &mut Screen, full: bool, body: bool) {
        let r = self.lay.g_freq;
        let th = &self.t;
        let s = &self.s;
        let maxf = s.t.maxf;
        let (_, avg, topf) = self.totals();
        let ir = r.inner();
        let (ga, gt) = (ghz(avg), ghz(topf));
        let (ca, ct) = (
            th.freq.at(avg / maxf) | BOLD,
            th.freq.at(topf / maxf) | BOLD,
        );
        let vs: [&[(&str, u32)]; 4] = [
            &[
                ("clock ", th.title | BOLD),
                ("avg ", th.mid),
                (ga.s(), ca),
                ("  top ", th.mid),
                (gt.s(), ct),
                (" GHz", th.mid),
            ],
            &[
                ("clock ", th.title | BOLD),
                (ga.s(), ca),
                (" / ", th.dim),
                (gt.s(), ct),
            ],
            &[("clock ", th.title | BOLD), (ga.s(), ca)],
            &[("clock", th.title | BOLD)],
        ];
        let title = Self::pick(r.w, &vs);
        if full {
            let win = span(2 * ir.w.saturating_sub(5));
            let sub = f!("average clock · last {}", win.s());
            self.panel(scr, r, title, sub.s());
        } else {
            self.panel_top(scr, r, title);
        }
        if !(full || body) {
            return;
        }
        let mut vals = std::mem::take(&mut self.vals);
        self.hist_vals(&mut vals, 2 * ir.w, 0.0, 1.0, Hist::Avg);
        let (m1, m2) = (ghz(maxf), ghz(maxf / 2.0));
        self.axis_graph(
            scr,
            ir.x,
            ir.y,
            ir.w,
            ir.h,
            &vals,
            maxf,
            &self.t.freq,
            &[(1.0, m1.s()), (0.5, m2.s()), (0.0, "0")],
        );
        self.vals = vals;
    }

    #[allow(clippy::too_many_arguments)]
    fn meter(
        &self,
        scr: &mut Screen,
        x: usize,
        y: usize,
        frac: f32,
        w: usize,
        ramp: &crate::color::Ramp,
        bg: u32,
        g: char,
    ) {
        let n = fill16(frac, w);
        for i in 0..w {
            Self::bar_cell(scr, x, y, i, n, w, ramp, bg, g);
        }
    }

    /// Cell `i` of a half-block bar filled to `n` sixteenths: the edge cell is the bar
    /// colour blended into the track by its covered fraction (sub-cell resolution).
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn bar_cell(
        scr: &mut Screen,
        x: usize,
        y: usize,
        i: usize,
        n: usize,
        w: usize,
        ramp: &crate::color::Ramp,
        bg: u32,
        g: char,
    ) {
        let den = (w.max(2) - 1) as f32;
        let fg = if i < n / 16 {
            ramp.at(i as f32 / den)
        } else if i == n / 16 && n % 16 > 0 {
            ramp.at(i as f32 / den) | ((n % 16) as u32) << BLEND_SHIFT
        } else {
            TRACK
        };
        scr.put(x + i, y, g, fg, bg);
    }

    /// Update a meter drawn earlier filled to `old` sixteenths: only the cells in between
    /// change (the gradient is fixed per position).
    #[allow(clippy::too_many_arguments)]
    fn meter_to(
        &self,
        scr: &mut Screen,
        x: usize,
        y: usize,
        old: usize,
        n: usize,
        w: usize,
        ramp: &crate::color::Ramp,
        bg: u32,
        g: char,
    ) {
        for i in old.min(n) / 16..(old.max(n) / 16 + 1).min(w) {
            Self::bar_cell(scr, x, y, i, n, w, ramp, bg, g);
        }
    }

    /// `parts`: 1 static rows, 2 live rows (temperatures, CCDs), 4 temperature history.
    fn draw_sensors(&mut self, scr: &mut Screen, full: bool, parts: u8) {
        let r = self.lay.sensors;
        if full {
            self.panel(scr, r, &[("sensors", self.t.title | BOLD)], "");
        }
        let ir = r.inner();
        if ir.h == 0 || ir.w < 10 {
            return;
        }
        let th = &self.t;
        let bg = surf(S_PANEL);
        let s = &self.s;
        // wide enough: bar + value + a history strip (4 s per cell) per sensor
        let tsw = if ir.w >= 32 { 8 } else { 0 };
        let bw = self.sensors_bw();
        let nt = s.temps.len().max(1);
        let skip_blank = nt + 5 > ir.h;
        let mut y = ir.y;
        let maxy = ir.y + ir.h;
        let maxx = ir.x + ir.w;
        if s.temps.is_empty() {
            Self::line(
                scr,
                ir.x,
                y,
                maxx,
                bg,
                &[("no CPU temperature sensor found", th.dim)],
            );
            y += 1;
        }
        for (k, sn) in s.temps.iter().enumerate() {
            if y >= maxy {
                break;
            }
            if parts & 3 == 0 {
                y += 1;
                continue;
            }
            let (pos, val) = self.temp(k);
            let x = (val - 30.0) / 65.0;
            let lab = f!("{:<6}", sn.label);
            scr.text(ir.x, y, lab.s(), th.mid, bg, ir.x + 6);
            self.meter(
                scr,
                ir.x + 6,
                y,
                (pos - 30.0) / 65.0,
                bw,
                &th.temp,
                bg,
                LOAD,
            );
            let v = num(" ", val, 4, "°C");
            Self::line(
                scr,
                ir.x + 6 + bw,
                y,
                maxx,
                bg,
                &[(v.s(), th.temp.at(x) | BOLD)],
            );
            if tsw > 0 {
                // the hottest reading in the history window, after the value
                let mx = sn.max.max(val);
                let m = num("", mx, 0, "°");
                let fm = ((mx - 30.0) / 65.0).clamp(0.0, 1.0);
                Self::line(
                    scr,
                    maxx - tsw,
                    y,
                    maxx,
                    bg,
                    &[("max ", th.dim), (m.s(), th.temp.at(fm))],
                );
            }
            y += 1;
        }
        if !skip_blank {
            y += 1;
        }
        let maxf = s.t.maxf;
        let peak = s.peak.iter().copied().fold(0.0, f32::max);
        let boosting =
            s.f.iter()
                .filter(|&&f| f >= s.t.boost && s.t.has_freq)
                .count();
        let busy = s.u.iter().filter(|&&u| u >= 0.5).count();
        let epp = if s.epp.is_empty() {
            f!("")
        } else {
            f!(" · {}", s.epp)
        };
        let la = {
            let mut f = F::new();
            for (i, &v) in s.loadavg.iter().enumerate() {
                if i > 0 {
                    f.str("  ");
                }
                f.str(ghz(v * 1000.0).s());
            }
            f
        };
        let b1 = f!("{busy}");
        let b2 = f!("/{} busy", s.u.len());
        let b3 = f!("{boosting}");
        let pk = ghz(peak);
        let of = f!("  of {}", ghz(maxf).s());
        let bst = if boosting > 0 { th.freq.at(1.0) } else { th.fg };
        let pks = th.freq.at(peak / maxf) | BOLD;
        // each row: variants, most detailed first; the first that fits is drawn
        let rows: [&[&[(&str, u32)]]; 4] = [
            &[
                &[("policy  ", th.dim), (&s.gov, th.text), (epp.s(), th.mid)],
                &[("policy  ", th.dim), (&s.gov, th.text)],
                &[(&s.gov, th.text)],
            ],
            &[
                &[("loadavg ", th.dim), (la.s(), th.text)],
                &[(la.s(), th.text)],
            ],
            &[
                &[
                    ("active  ", th.dim),
                    (b1.s(), th.fg),
                    (b2.s(), th.mid),
                    ("  ", 0),
                    (b3.s(), bst),
                    (" boosting", th.mid),
                ],
                &[
                    ("active  ", th.dim),
                    (b1.s(), th.fg),
                    (b2.s(), th.mid),
                    ("  ", 0),
                    (b3.s(), bst),
                    (" boost", th.mid),
                ],
                &[
                    (b1.s(), th.fg),
                    (b2.s(), th.mid),
                    ("  ", 0),
                    (b3.s(), bst),
                    (" boost", th.mid),
                ],
                &[(b1.s(), th.fg), (b2.s(), th.mid)],
            ],
            &[
                &[
                    ("peak    ", th.dim),
                    (pk.s(), pks),
                    (" GHz", th.mid),
                    (of.s(), th.dim),
                ],
                &[("peak    ", th.dim), (pk.s(), pks), (" GHz", th.mid)],
                &[("peak ", th.dim), (pk.s(), pks)],
            ],
        ];
        for vs in rows {
            if y >= maxy {
                break;
            }
            if parts & 1 == 0 {
                y += 1;
                continue;
            }
            let sp = vs
                .iter()
                .find(|v| Self::spans_len(v) <= ir.w)
                .unwrap_or(&vs[vs.len() - 1]);
            Self::line(scr, ir.x, y, maxx, bg, sp);
            y += 1;
        }
        // spare height: one line per CCD (load bar, mean load, mean core clock)
        let nd = s.t.ndom;
        if nd > 1 && y + 1 + nd <= maxy && ir.w >= 24 {
            y += 1;
            for d in 0..nd {
                if parts & 2 == 0 {
                    y += 1;
                    continue;
                }
                let (bu, u, f) = self.ccd(d);
                let lab = f!("CCD{d}");
                scr.text(ir.x, y, lab.s(), th.mid, bg, ir.x + 6);
                for x in ir.x + lab.n..ir.x + 6 {
                    scr.put(x, y, ' ', 0, bg);
                }
                self.meter(scr, ir.x + 6, y, bu, bw, &th.load, bg, LOAD);
                let pc = num(" ", u * 100.0, 3, "%");
                let gz = ghz(f);
                Self::line(
                    scr,
                    ir.x + 6 + bw,
                    y,
                    maxx,
                    bg,
                    &[
                        (pc.s(), th.load.at(u)),
                        ("  ", 0),
                        (gz.s(), self.freq_style(f, f <= s.t.minf * 1.05 + 1.0)),
                        (" GHz", th.dim),
                    ],
                );
                y += 1;
            }
        }
        // spare height: temperature history
        let gh = maxy.saturating_sub(y + 2);
        if let (Some(sn), true) = (s.temps.first(), gh >= 2 && parts & 4 != 0) {
            y += 1;
            let win = span(2 * ir.w.saturating_sub(5));
            let a = f!("{} history", sn.label);
            let b = f!(" · last {}", win.s());
            Self::line(scr, ir.x, y, maxx, bg, &[(a.s(), th.mid), (b.s(), th.dim)]);
            y += 1;
            let mut vals = std::mem::take(&mut self.vals);
            self.hist_vals(&mut vals, 2 * ir.w, 0.0, 1.0, Hist::Temp(0));
            for v in vals.iter_mut() {
                *v = (*v - 30.0).max(0.0);
            }
            self.axis_graph(
                scr,
                ir.x,
                y,
                ir.w,
                gh,
                &vals,
                65.0,
                &self.t.temp,
                &[(1.0, "95°"), (0.5, "63°"), (0.0, "30°")],
            );
            self.vals = vals;
        }
    }

    /// Field positions of a core card `width` cells wide in card mode `mode`.
    pub fn geo(&self, width: usize, mode: u8) -> Geo {
        let idw = self.idw;
        // bars grow slowly with the width, the load-history strip takes the rest
        let split = |avail: usize| -> (usize, usize, usize) {
            if avail >= 36 {
                let bars = (26 + (avail - 36) * 3 / 10).min(66);
                let fw = bars * 47 / 100;
                (fw, bars - fw, avail - 2 - bars)
            } else {
                let a = avail.max(8);
                let fw = (a * 47 / 100).max(4);
                (fw, a.saturating_sub(fw).max(3), 0)
            }
        };
        match mode {
            M_MICRO => {
                let ghz = width >= 16;
                let xl = idw + 2;
                let fixed = xl + 4 + if ghz { 4 } else { 0 };
                let lw = width.saturating_sub(fixed).max(3);
                let xn = xl + lw + 1;
                Geo {
                    lw,
                    xl,
                    xn,
                    xp: if ghz { xn + 4 } else { xn },
                    ghz,
                    ..Default::default()
                }
            }
            M_COMPACT => {
                let xb = idw + 3;
                let (fw, lw, sw) = split(width.saturating_sub(idw + 18));
                let xn = xb + fw + 1;
                let xl = xn + 6;
                let xp = xl + lw + 1;
                Geo {
                    fw,
                    lw,
                    sw,
                    xb,
                    xn,
                    xl,
                    xp,
                    xs: xp + 9,
                    ghz: true,
                }
            }
            _ => {
                let xb = 2 * idw + 5;
                let (fw, lw, sw) = split(width.saturating_sub(2 * idw + 17));
                let xn = xb + fw + 1;
                let xl = xn + 6;
                let xp = xl + lw + 1;
                Geo {
                    fw,
                    lw,
                    sw,
                    xb,
                    xn,
                    xl,
                    xp,
                    xs: xp + 6,
                    ghz: true,
                }
            }
        }
    }

    /// A core's clock for compact cards: the fastest of its threads (one clock domain).
    fn core_clock(&self, thr: &[usize]) -> (f32, f32, bool) {
        let mut f = 0.0f32;
        let mut b = 0.0f32;
        let mut idle = true;
        for &c in thr {
            let (fc, u) = self.shown(c);
            f = f.max(fc);
            b = b.max(self.bf[c]);
            idle &= u < 0.01 && fc <= self.s.t.minf * 1.05 + 1.0;
        }
        (f, b, idle)
    }

    /// The scheduler's preferred core(s): the highest amd-pstate ranking (ties too).
    fn preferred(&self, g: usize) -> bool {
        let r = &self.s.t.rank;
        r[g] >= 0 && r.iter().all(|&x| x <= r[g]) && r.iter().any(|&x| x != r[g])
    }

    /// Selection / preferred-core marker before a core's name.
    fn marker(&self, g: usize) -> (char, u32) {
        if g == self.selected {
            ('▌', self.t.accent)
        } else if self.preferred(g) {
            ('◆', self.t.warn)
        } else {
            (' ', 0)
        }
    }

    fn freq_style(&self, f: f32, idle: bool) -> u32 {
        let th = &self.t;
        let maxf = self.s.t.maxf;
        // not bold: clocks are secondary to load (most busy threads boost anyway)
        if f >= self.s.t.boost {
            th.freq.at(f / maxf)
        } else if idle {
            th.dim
        } else {
            th.freq.at((f / maxf).max(0.45))
        }
    }

    fn draw_cores(&mut self, scr: &mut Screen, full: bool) {
        let l = &self.lay;
        let th = &self.t;
        let n = self.ngroups();
        let ir = l.cores.inner();
        if full {
            let view = ["full", "compact", "micro"][l.mode as usize];
            let vtxt = if self.view == 0 {
                f!("auto · {view}")
            } else {
                f!("{view}")
            };
            let nn = f!("{n}");
            let pref = l.mode != M_MICRO && (0..n).any(|g| self.preferred(g));
            let dual = l.compact && self.s.t.groups.first().is_some_and(|g| g.len() >= 2);
            let subs: [&str; 4] = [
                if pref && dual {
                    "◆ preferred core · faded load = one thread"
                } else {
                    ""
                },
                if dual { "faded load = one thread" } else { "" },
                if pref { "◆ preferred core" } else { "" },
                "",
            ];
            let sub = subs
                .iter()
                .find(|t| !t.is_empty() && t.chars().count() + 8 < l.cores.w)
                .copied()
                .unwrap_or("");
            let vs: [&[(&str, u32)]; 4] = [
                &[
                    ("cores ", th.title | BOLD),
                    (nn.s(), th.text),
                    ("   sort ", th.mid),
                    (SORTS[self.sort], th.text),
                    ("   view ", th.mid),
                    (vtxt.s(), th.text),
                ],
                &[
                    ("cores ", th.title | BOLD),
                    (nn.s(), th.text),
                    ("   sort ", th.mid),
                    (SORTS[self.sort], th.text),
                    ("  ", 0),
                    (view, th.text),
                ],
                &[
                    ("cores ", th.title | BOLD),
                    (nn.s(), th.text),
                    ("  ", 0),
                    (SORTS[self.sort], th.text),
                ],
                &[("cores ", th.title | BOLD), (nn.s(), th.text)],
            ];
            self.panel(scr, l.cores, Self::pick(l.cores.w, &vs), sub);
            // CCD boundaries: a thin rule in the gutter between columns of different CCDs
            for c in 1..l.cols {
                let d = |c: usize| self.order.get(c * l.nrows).map(|&k| self.s.t.ccd[k]);
                let rule = d(c) != d(c - 1) || self.sort != 0;
                if rule {
                    let (px, pw) = l.card_x[c - 1];
                    let x = ir.x + (px + pw + l.card_x[c].0) / 2;
                    for y in ir.y..ir.y + ir.h {
                        scr.put(x, y, '│', th.border, surf(S_PANEL));
                    }
                }
            }
        }
        if ir.h == 0 {
            return;
        }
        if l.header || l.div {
            let mut k = mixk(&[
                self.sort as u64,
                l.mode as u64,
                self.s.hist_n.min(4096) / 64,
            ]);
            for d in 0..self.s.t.ndom {
                k = mixk(&[k, self.dom_temp(d).map_or(0, |t| t.round() as u64)]);
            }
            if self.memo(SLOT_HEADER, k) {
                self.draw_cores_header(scr, ir);
            }
        }
        let l = self.lay.clone();
        for c in 0..l.cols {
            let (cx, cw) = l.card_x[c];
            let geo = self.geo(cw, l.mode);
            for vr in 0..l.vis_rows {
                let r = vr + self.scroll;
                if r >= l.nrows {
                    break;
                }
                let k = c * l.nrows + r;
                let Some(&g) = self.order.get(k) else {
                    continue;
                };
                let y = self.row_y(c, vr);
                if y + l.rh > ir.y + ir.h {
                    break;
                }
                let s = &self.s;
                // card position / selection / (compact) values: a change redraws the card
                let mut key = [0u64; 12];
                key[0] = (g == self.selected) as u64 | (k as u64) << 1 | (y as u64) << 20;
                if l.compact {
                    key[1] = if geo.sw > 0 { self.strip_key(g) } else { 0 };
                    let (f, b, idle) = self.core_clock(&s.t.groups[g]);
                    key[2] =
                        (f / 10.0) as u64 | (fill16(b, geo.fw) as u64) << 32 | (idle as u64) << 60;
                    for (j, &t) in s.t.groups[g].iter().take(4).enumerate() {
                        let u = self.shown(t).1;
                        key[3 + 2 * j] =
                            (u * 1000.0) as u64 | (fill16(self.bu[t], geo.lw) as u64) << 32;
                        key[4 + 2 * j] = (self.shown(t).0 / 10.0) as u64;
                    }
                }
                let force = self.memo(SLOT_CARDS + g, mixk(&key));
                if l.compact {
                    if force {
                        self.draw_card_compact(scr, g, ir.x + cx, y, cw, &geo, l.mode);
                    }
                } else {
                    let mut rm = std::mem::take(&mut self.row_memo);
                    if rm.len() < self.s.f.len() {
                        rm.resize(self.s.f.len(), [u32::MAX; 6]);
                    }
                    self.draw_card(scr, g, ir.x + cx, y, cw, &geo, l.rh, &mut rm, force);
                    self.row_memo = rm;
                }
            }
        }
    }

    fn draw_cores_header(&self, scr: &mut Screen, ir: Rect) {
        let l = &self.lay;
        let th = &self.t;
        let s = &self.s;
        let bg = surf(S_PANEL);
        for c in 0..l.cols {
            let (cx, cw) = l.card_x[c];
            let x0 = ir.x + cx;
            let maxx = x0 + cw;
            let mut dom = usize::MAX;
            let mut multi = false;
            for r in 0..l.nrows {
                if let Some(&g) = self.order.get(c * l.nrows + r) {
                    let d = s.t.ccd[g];
                    if dom == usize::MAX {
                        dom = d;
                    } else if dom != d {
                        multi = true;
                    }
                }
            }
            let geo = self.geo(cw, l.mode);
            let lead = if l.mode == M_MICRO { "" } else { " " };
            let label = |d: usize, scr: &mut Screen, y: usize| -> usize {
                let lab = f!("{lead}CCD{d}");
                let mut x = scr.text(x0, y, lab.s(), th.text | BOLD, bg, maxx);
                if let Some(temp) = self.dom_temp(d) {
                    let tt = num(" ", temp, 0, "°");
                    x = scr.text(x, y, tt.s(), th.temp.at((temp - 30.0) / 65.0), bg, maxx);
                }
                x
            };
            if let (true, Some(sr)) = (l.div, self.split_in(c, l.nrows)) {
                if sr >= self.scroll && sr - self.scroll < l.vis_rows {
                    let y = self.row_y(c, sr - self.scroll) - 1;
                    let d = self.order.get(c * l.nrows + sr).map_or(0, |&g| s.t.ccd[g]);
                    let mut x = label(d, scr, y);
                    x = scr.text(x, y, " ", 0, bg, maxx);
                    while x < maxx {
                        scr.put(x, y, '─', th.border, bg);
                        x += 1;
                    }
                }
            }
            if !l.header {
                continue;
            }
            let first = self.order.get(c * l.nrows).map(|&g| s.t.ccd[g]);
            let mut x;
            if let (true, Some(d)) = (l.div && self.sort == 0, first) {
                x = label(d, scr, ir.y);
            } else if s.t.ndom > 1 && !multi && dom != usize::MAX && self.sort == 0 {
                let lab = f!("{lead}CCD{dom}");
                x = scr.text(x0, ir.y, lab.s(), th.text | BOLD, bg, maxx);
                if let Some(temp) = self.dom_temp(dom) {
                    let tt = num(" ", temp, 0, "°");
                    x = scr.text(x, ir.y, tt.s(), th.temp.at((temp - 30.0) / 65.0), bg, maxx);
                }
            } else {
                let lab = f!("{lead}core");
                x = scr.text(x0, ir.y, lab.s(), th.dim, bg, maxx);
            }
            // a label goes exactly over its column, or not at all
            let mut at = |col: usize, text: &str, x: &mut usize| {
                let col = x0 + col;
                if col <= *x || col + text.chars().count() > maxx {
                    return;
                }
                while *x < col {
                    scr.put(*x, ir.y, ' ', 0, bg);
                    *x += 1;
                }
                *x = scr.text(col, ir.y, text, th.dim, bg, maxx);
            };
            if l.mode == M_MICRO {
                at(geo.xl, "load", &mut x);
                if geo.ghz {
                    at(geo.xn, "GHz", &mut x);
                }
                at(geo.xp + 2, "%", &mut x);
            } else {
                // "clock" sits at the bar's start, or at its end when the CCD label is there
                if x0 + geo.xb > x {
                    at(geo.xb, "clock", &mut x);
                } else if geo.fw >= 12 {
                    at(geo.xb + geo.fw - 5, "clock", &mut x);
                }
                at(geo.xn + 1, "GHz", &mut x);
                at(geo.xl, "load", &mut x);
                at(
                    geo.xp + if l.mode == M_COMPACT { 6 } else { 3 },
                    "%",
                    &mut x,
                );
                if geo.sw > 0 {
                    let win = span(geo.sw * STRIP_SECS);
                    let lab = f!("load history · {}", win.s());
                    let lab = if lab.n <= geo.sw { lab } else { f!("history") };
                    at(geo.xs, lab.s(), &mut x);
                }
            }
            while x < maxx {
                scr.put(x, ir.y, ' ', 0, bg);
                x += 1;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_card(
        &self,
        scr: &mut Screen,
        g: usize,
        x0: usize,
        y0: usize,
        w: usize,
        geo: &Geo,
        rh: usize,
        rm: &mut [[u32; 6]],
        force: bool,
    ) {
        let sel = g == self.selected;
        let bg = if sel { surf(S_SELECT) } else { surf(S_PANEL) };
        let threads = &self.s.t.groups[g];
        let th = &self.t;
        let s = &self.s;
        let maxx = x0 + w;
        let (fw, lw, sw) = (geo.fw, geo.lw, geo.sw);
        let idw = self.idw;
        let active = threads.iter().any(|&c| self.shown(c).1 >= 0.05);
        for j in 0..rh {
            let y = y0 + j;
            let Some(&c) = threads.get(j) else {
                if force {
                    Self::line(scr, x0, y, maxx, bg, &[]);
                }
                continue;
            };
            let (f, u) = self.shown(c);
            let idle = u < 0.01 && f <= s.t.minf * 1.05 + 1.0;
            let (bf, bu) = (self.bf[c], self.bu[c]);
            let fnn = fill16(bf, fw) as u32;
            let lnn = fill16(bu, lw) as u32;
            let pct = (u * 100.0).round() as u32;
            let keys = [
                fnn | (idle as u32) << 16,
                (f / 10.0) as u32 | (idle as u32) << 24,
                lnn,
                pct | ((u * 64.0).round() as u32) << 8,
                if sw > 0 { self.strip_key(g) as u32 } else { 0 },
                (j == 0 && (sel || active)) as u32,
            ];
            let mut memo = if force || c >= rm.len() {
                [u32::MAX; 6]
            } else {
                rm[c]
            };
            if force {
                // static parts of the row: marker, names, separators
                let mut x = x0;
                let (mc, mf) = if j == 0 || sel {
                    self.marker(g)
                } else {
                    (' ', 0)
                };
                scr.put(x, y, mc, mf, bg);
                x += 1 + idw + 1;
                let tid = f!(" t{:0w$} ", s.t.cpus[c], w = idw);
                x = scr.text(x, y, tid.s(), th.dim, bg, maxx);
                x += fw;
                scr.put(x, y, ' ', 0, bg);
                x += 5;
                x = scr.text(x, y, "  ", 0, bg, maxx);
                x += lw;
                scr.put(x, y, ' ', 0, bg);
                x += 4;
                x = scr.text(x, y, "%", th.dim, bg, maxx);
                if sw > 0 {
                    x = scr.text(x, y, "  ", 0, bg, maxx) + sw;
                }
                while x < maxx {
                    scr.put(x, y, ' ', 0, bg);
                    x += 1;
                }
                if j > 0 {
                    for k in 0..idw + 1 {
                        scr.put(x0 + 1 + k, y, ' ', 0, bg);
                    }
                }
            }
            let xb = x0 + geo.xb;
            if j == 0 && memo[5] != keys[5] {
                let name = f!("C{:0w$}", g, w = idw);
                scr.text(
                    x0 + 1,
                    y,
                    name.s(),
                    if keys[5] == 1 { th.fg } else { th.mid } | BOLD,
                    bg,
                    maxx,
                );
            }
            if memo[0] != keys[0] {
                let ramp = if idle { &th.freq_idle } else { &th.freq };
                if memo[0] != u32::MAX && memo[0] >> 16 == keys[0] >> 16 {
                    self.meter_to(
                        scr,
                        xb,
                        y,
                        (memo[0] & 0xffff) as usize,
                        fnn as usize,
                        fw,
                        ramp,
                        bg,
                        CLK,
                    );
                } else {
                    self.meter(scr, xb, y, bf, fw, ramp, bg, CLK);
                }
            }
            if memo[1] != keys[1] {
                let gz = ghz(f);
                scr.text(x0 + geo.xn, y, gz.s(), self.freq_style(f, idle), bg, maxx);
            }
            let xl = x0 + geo.xl;
            if memo[2] != keys[2] {
                if memo[2] != u32::MAX {
                    self.meter_to(
                        scr,
                        xl,
                        y,
                        memo[2] as usize,
                        lnn as usize,
                        lw,
                        &th.load,
                        bg,
                        LOAD,
                    );
                } else {
                    self.meter(scr, xl, y, bu, lw, &th.load, bg, LOAD);
                }
            }
            if memo[3] != keys[3] {
                let pc = num("", u * 100.0, 3, "");
                let uc = if u < 0.01 {
                    th.dim
                } else if u >= 0.9 {
                    th.load.at(u) | BOLD
                } else {
                    th.load.at(u)
                };
                scr.text(x0 + geo.xp, y, pc.s(), uc, bg, maxx);
            }
            if sw > 0 && memo[4] != keys[4] {
                self.strip(scr, x0 + geo.xs, y, &threads[j..j + 1], sw, bg);
            }
            memo = keys;
            if c < rm.len() {
                rm[c] = memo;
            }
        }
    }

    /// One row per core: compact (core clock bar, two-thread load bar, both loads,
    /// history strip) or micro (load bar, clock, busiest thread's load).
    #[allow(clippy::too_many_arguments)]
    fn draw_card_compact(
        &self,
        scr: &mut Screen,
        g: usize,
        x0: usize,
        y: usize,
        w: usize,
        geo: &Geo,
        mode: u8,
    ) {
        let th = &self.t;
        let thr = &self.s.t.groups[g];
        let sel = g == self.selected;
        let bg = if sel { surf(S_SELECT) } else { surf(S_PANEL) };
        let maxx = x0 + w;
        let (f, bf, idle) = self.core_clock(thr);
        let ta = thr[0];
        let tb = thr.get(1).copied();
        let ua = self.shown(ta).1;
        let ub = tb.map_or(0.0, |t| self.shown(t).1);
        let active = ua.max(ub) >= 0.05;
        let mut x = x0;
        let name = f!("C{:0w$}", g, w = self.idw);
        if mode == M_MICRO {
            let nc = if sel || active { th.fg } else { th.mid } | BOLD;
            x = scr.text(x, y, name.s(), nc, bg, maxx);
        } else {
            let (mc, mf) = self.marker(g);
            scr.put(x, y, mc, mf, bg);
            x = scr.text(
                x + 1,
                y,
                name.s(),
                if sel || active { th.fg } else { th.mid } | BOLD,
                bg,
                maxx,
            );
            while x < x0 + geo.xb {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
            let ramp = if idle { &th.freq_idle } else { &th.freq };
            self.meter(scr, x, y, bf, geo.fw, ramp, bg, CLK);
            x = scr.text(x + geo.fw, y, " ", 0, bg, maxx);
            let gz = ghz(f);
            x = scr.text(x, y, gz.s(), self.freq_style(f, idle), bg, maxx);
        }
        while x < x0 + geo.xl {
            scr.put(x, y, ' ', 0, bg);
            x += 1;
        }
        match tb {
            Some(tb) => self.split_meter(scr, x, y, self.bu[ta], self.bu[tb], geo.lw, bg),
            None => self.meter(scr, x, y, self.bu[ta], geo.lw, &th.load, bg, LOAD),
        }
        x += geo.lw;
        let pc_style = |u: f32| if u < 0.01 { th.dim } else { th.load.at(u) };
        if mode == M_MICRO {
            if geo.ghz {
                x = scr.text(x, y, " ", 0, bg, maxx);
                let gz = ghz(f);
                let st = if idle {
                    th.dim
                } else {
                    self.freq_style(f, false)
                };
                x = scr.text(x, y, &gz.s()[..3], st, bg, maxx);
            }
            let u = ua.max(ub);
            let pc = num(" ", u * 100.0, 3, "");
            x = scr.text(x, y, pc.s(), pc_style(u), bg, maxx);
        } else {
            for u in [Some(ua), tb.map(|_| ub)] {
                match u {
                    Some(u) => {
                        let pc = num(" ", u * 100.0, 3, "");
                        x = scr.text(x, y, pc.s(), pc_style(u), bg, maxx);
                    }
                    None => x = scr.text(x, y, "    ", 0, bg, maxx),
                }
            }
            if geo.sw > 0 {
                x = scr.text(x, y, "  ", 0, bg, maxx);
                self.strip(scr, x, y, thr, geo.sw, bg);
                x += geo.sw;
            }
        }
        while x < maxx {
            scr.put(x, y, ' ', 0, bg);
            x += 1;
        }
    }

    /// A core's two threads in one thin bar: full colour up to the less busy thread's
    /// load, faded from there to the busier one's (edge cells blend, as in `meter`).
    #[allow(clippy::too_many_arguments)]
    fn split_meter(&self, scr: &mut Screen, x: usize, y: usize, a: f32, b: f32, w: usize, bg: u32) {
        let (lo, hi) = (fill16(a.min(b), w), fill16(a.max(b), w));
        let den = (w.max(2) - 1) as f32;
        let (full, one) = (&self.t.load, &self.t.load_one);
        for i in 0..w {
            let t = i as f32 / den;
            // whole cells of the lower bar; its edge cell counts if at least half full
            let in_lo = i < lo / 16 || (i == lo / 16 && lo % 16 >= 8);
            let fg = if i < hi / 16 {
                if in_lo {
                    full.at(t)
                } else {
                    one.at(t)
                }
            } else if i == hi / 16 && hi % 16 > 0 {
                (if in_lo { full.at(t) } else { one.at(t) }) | ((hi % 16) as u32) << BLEND_SHIFT
            } else {
                TRACK
            };
            scr.put(x + i, y, LOAD, fg, bg);
        }
    }

    /// Smooth mode: mean load of `thr` per history cell (`STRIP_SECS` each, newest
    /// last) from the scrolling curve, into `out` (`w` values). Returns how many cells
    /// (from the right) the history covers.
    fn cells_smooth(&self, out: &mut Vec<f32>, w: usize, thr: &[usize]) -> usize {
        const K: usize = 4; // samples per cell
        let st = STRIP_SECS as f32 / K as f32;
        self.hist_vals(out, w * K, st / 2.0, st, Hist::Load(thr));
        for i in 0..w {
            out[i] = out[i * K..i * K + K].iter().sum::<f32>() / K as f32;
        }
        out.truncate(w);
        let len = thr.iter().map(|&c| self.s.hu[c].len()).min().unwrap_or(0);
        (((len as f32 + self.phase) / STRIP_SECS as f32) as usize).min(w)
    }

    /// Load history of `thr` (averaged) as a strip of eighth-blocks, newest on the
    /// right, `STRIP_SECS` per cell; buckets are aligned to whole cells so the strip
    /// moves once per cell, not with every point.
    #[allow(clippy::too_many_arguments)]
    fn strip(&self, scr: &mut Screen, x: usize, y: usize, thr: &[usize], w: usize, bg: u32) {
        if self.smooth() {
            let mut v = self.scratch.borrow_mut();
            let have = self.cells_smooth(&mut v, w, thr);
            for i in 0..w {
                let xx = x + w - 1 - i;
                if i >= have {
                    scr.put(xx, y, ' ', NOPATCH, bg);
                    continue;
                }
                let u = v[w - 1 - i];
                let lv = (u * 4.0).round() as usize;
                if lv == 0 {
                    scr.put(xx, y, '▁', TRACK, bg);
                } else {
                    let q = (u * 16.0).round() / 16.0;
                    scr.put(xx, y, BLOCKS[lv.min(4) - 1], self.t.load.at(q), bg);
                }
            }
            return;
        }
        let s = &self.s;
        let k = STRIP_SECS;
        let lag = (s.hist_n % k as u64) as usize;
        let len = thr.iter().map(|&c| s.hu[c].len()).min().unwrap_or(0);
        let ramp = &self.t.load;
        for i in 0..w {
            // cell i from the right
            let (a, b) = (lag + i * k, lag + (i + 1) * k);
            let xx = x + w - 1 - i;
            if b > len {
                scr.put(xx, y, ' ', NOPATCH, bg);
                continue;
            }
            let mut sum = 0u32;
            for &c in thr {
                for p in a..b {
                    sum += s.hu[c].back(p) as u32;
                }
            }
            let v = sum as f32 / (k * thr.len()) as f32 / 255.0;
            // half a cell at most, like the bars, so neighbouring rows stay apart
            let lv = (v * 4.0).round() as usize;
            if lv == 0 {
                scr.put(xx, y, '▁', TRACK, bg);
            } else {
                scr.put(xx, y, BLOCKS[lv.min(4) - 1], ramp.at(v), bg);
            }
        }
    }

    /// Per-core history grid: (cells x, y, w, h of the grid area, rows, cols, cell w, cell h).
    fn grid_geo(&self, r: Rect) -> Option<(Rect, usize, usize, usize, usize)> {
        let n = self.ngroups();
        let ir = r.inner();
        let gw = ir.w.saturating_sub(GRID_STATS + 3);
        let mut best = (f32::MAX, 0, 0, 0, 0);
        for rows in 1..=4usize.min(n) {
            let cols = n.div_ceil(rows);
            let cw = ((gw + 2) / cols).saturating_sub(2);
            let ch = ((ir.h + 1) / rows).saturating_sub(1);
            if cw < 16 || ch < 5 {
                continue;
            }
            // landscape cells (a cell is ~1:2), and full rows
            let aspect = cw as f32 / (2.0 * ch as f32);
            let score = (aspect - 1.5).abs() + if n % rows != 0 { 1.0 } else { 0.0 };
            if score < best.0 {
                best = (score, rows, cols, cw, ch);
            }
        }
        let g = Rect {
            x: ir.x,
            y: ir.y,
            w: gw,
            h: ir.h,
        };
        (best.1 > 0).then_some((g, best.1, best.2, best.3, best.4))
    }

    /// Which core's grid cell is at screen cell (x, y)?
    fn grid_at(&self, x: usize, y: usize) -> Option<usize> {
        let r = self.lay.det.filter(|_| self.lay.grid)?;
        let (g, _, cols, cw, ch) = self.grid_geo(r)?;
        if !g.contains(x, y) {
            return None;
        }
        let (c, rr) = ((x - g.x) / (cw + 2), (y - g.y) / (ch + 1));
        let k = rr * cols + c;
        (k < self.ngroups()).then_some(k)
    }

    fn draw_grid(&mut self, scr: &mut Screen, r: Rect, full: bool, body: bool, stats: bool) {
        let th = &self.t;
        let Some((g, rows, cols, cw, ch)) = self.grid_geo(r) else {
            return;
        };
        let win = span(2 * cw);
        if full {
            let sub = f!("last {} per core · click to select", win.s());
            self.panel(
                scr,
                r,
                &[
                    ("history ", th.title | BOLD),
                    ("clock", th.freq.at(0.85)),
                    (" over ", th.dim),
                    ("load", th.load.at(0.6)),
                ],
                sub.s(),
            );
        }
        let bg = surf(S_PANEL);
        let maxf = self.s.t.maxf;
        let mut vals = std::mem::take(&mut self.vals);
        let mut v16 = std::mem::take(&mut self.tmp16);
        let mut v8 = std::mem::take(&mut self.tmp8);
        let n = self.ngroups();
        for k in 0..n.min(rows * cols) {
            let (x, y) = (g.x + (k % cols) * (cw + 2), g.y + (k / cols) * (ch + 1));
            let thr = &self.s.t.groups[k];
            let sel = k == self.selected;
            let tbg = if sel { surf(S_SELECT) } else { bg };
            let (f, _, idle) = self.core_clock(thr);
            let u = thr.iter().map(|&c| self.shown(c).1).sum::<f32>() / thr.len() as f32;
            let name = f!("C{:0w$}", k, w = self.idw);
            let gz = ghz(f);
            let pc = num("", u * 100.0, 3, "%");
            let (mc, mf) = self.marker(k);
            scr.put(x, y, mc, mf, tbg);
            let mut xx = scr.text(
                x + 1,
                y,
                name.s(),
                if sel || u >= 0.05 { th.fg } else { th.mid } | BOLD,
                tbg,
                x + cw,
            );
            // value block right-aligned: "5.64 GHz  99%"
            let right = gz.n + 4 + 2 + pc.n;
            let rx = (x + cw).saturating_sub(right).max(xx + 1);
            while xx < rx {
                scr.put(xx, y, ' ', 0, tbg);
                xx += 1;
            }
            Self::line(
                scr,
                rx,
                y,
                x + cw,
                tbg,
                &[
                    (gz.s(), self.freq_style(f, idle)),
                    (" GHz  ", th.dim),
                    (pc.s(), if u < 0.01 { th.dim } else { th.load.at(u) }),
                ],
            );
            if !(full || body) {
                continue;
            }
            // clock (max of the threads) above load (mean of the threads)
            let gh = ch - 1;
            let fh = (gh * 2 / 5).max(1);
            let lh = gh - fh;
            self.hist_vals(&mut vals, 2 * cw, 0.0, 1.0, Hist::Clock(thr));
            self.graph(scr, x, y + 1, cw, fh, &vals, maxf, &th.freq);
            self.hist_vals(&mut vals, 2 * cw, 0.0, 1.0, Hist::Load(thr));
            self.graph(scr, x, y + 1 + fh, cw, lh, &vals, 1.0, &th.load);
        }
        // the selected core's numbers on the right
        if stats {
            let ir = r.inner();
            let sx = ir.x + ir.w - GRID_STATS;
            for yy in 0..ir.h {
                scr.put(sx - 2, ir.y + yy, '┃', th.border, bg);
            }
            self.draw_stats(
                scr,
                self.selected,
                sx,
                ir.y,
                GRID_STATS,
                ir.h,
                2 * cw,
                &mut v16,
                &mut v8,
            );
        }
        self.vals = vals;
        self.tmp16 = v16;
        self.tmp8 = v8;
    }

    /// Mean load of `thr` in history cell `i` (from the right, `STRIP_SECS` per cell,
    /// aligned to whole cells); None before the history starts.
    fn cell_load(&self, thr: &[usize], i: usize) -> Option<f32> {
        let s = &self.s;
        let k = STRIP_SECS;
        let lag = (s.hist_n % k as u64) as usize;
        let (a, b) = (lag + i * k, lag + (i + 1) * k);
        if thr.iter().any(|&c| b > s.hu[c].len()) {
            return None;
        }
        let mut sum = 0u32;
        for &c in thr {
            for p in a..b {
                sum += s.hu[c].back(p) as u32;
            }
        }
        Some(sum as f32 / (k * thr.len()) as f32 / 255.0)
    }

    /// The all-cores heatmap needs a row per two cores.
    fn heat_fits(&self, r: Rect) -> bool {
        let ir = r.inner();
        let n = self.ngroups();
        n >= 4 && ir.h >= n.div_ceil(2) && ir.w >= 60
    }

    /// Detail area as a load heatmap of every core over time (two cores per row:
    /// upper and lower half of each cell), the selected core's statistics beside it.
    fn draw_heat(&mut self, scr: &mut Screen, r: Rect, full: bool, stats: bool) {
        let (mut ca, mut cb) = (
            std::mem::take(&mut self.vals),
            std::mem::take(&mut self.vals2),
        );
        let th = &self.t;
        let ir = r.inner();
        let n = self.ngroups();
        let rows = n.div_ceil(2);
        let stats_w = if ir.w >= 110 { 30 } else { 0 };
        let idw = self.idw;
        let lw = 2 * idw + 3;
        let hx = ir.x + lw;
        let hw = ir.w - lw - if stats_w > 0 { stats_w + 3 } else { 0 };
        // a divider row between CCDs and a time axis, when there is room
        let ccd_at = (1..n).find(|&g| self.s.t.ccd[g] != self.s.t.ccd[g - 1]);
        let div = ccd_at.filter(|&g| g % 2 == 0 && ir.h > rows).map(|g| g / 2);
        let axis = ir.h > rows + div.is_some() as usize;
        if full {
            let win = span(hw * STRIP_SECS);
            let sub = f!("{} s per column · last {}", STRIP_SECS, win.s());
            self.panel(
                scr,
                r,
                &[("history ", th.title | BOLD), ("load per core", th.mid)],
                sub.s(),
            );
        }
        let bg = surf(S_PANEL);
        let track = th.track;
        let heat = |v: Option<f32>| -> u32 {
            match v {
                // quantised so a steady load gives identical neighbours (no repaint)
                Some(v) if v >= 0.03 => {
                    let q = (v * 16.0).round() / 16.0;
                    crate::color::mix(track, th.temp.at(q), 0.45 + 0.55 * q.min(1.0))
                }
                Some(_) => track,
                None => crate::color::mix(track, th.panel, 0.6),
            }
        };
        let mut y = ir.y;
        for k in 0..rows {
            if div == Some(k) {
                let d = self.s.t.ccd[2 * k];
                let lab = f!("CCD{d}");
                let mut x = scr.text(ir.x, y, lab.s(), th.dim, bg, hx);
                while x < hx + hw {
                    scr.put(x, y, if x >= hx { '─' } else { ' ' }, th.border, bg);
                    x += 1;
                }
                y += 1;
            }
            let (ga, gb) = (2 * k, 2 * k + 1);
            // label "C00·01", the selected core's number in the accent colour
            let num_st = |g: usize| {
                if g == self.selected {
                    th.accent | BOLD
                } else {
                    th.mid
                }
            };
            let na = f!("{:0w$}", ga, w = idw);
            let nb = f!("{:0w$}", gb, w = idw);
            let mut x = scr.text(ir.x, y, "C", th.dim, bg, hx);
            x = scr.text(x, y, na.s(), num_st(ga), bg, hx);
            if gb < n {
                x = scr.text(x, y, "·", th.dim, bg, hx);
                x = scr.text(x, y, nb.s(), num_st(gb), bg, hx);
            }
            while x < hx {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
            let ta = &self.s.t.groups[ga];
            let smooth = self.smooth();
            let (mut ha, mut hb) = (0, 0);
            if smooth {
                ha = self.cells_smooth(&mut ca, hw, ta);
                if gb < n {
                    hb = self.cells_smooth(&mut cb, hw, &self.s.t.groups[gb]);
                }
            }
            for i in 0..hw {
                let va = if smooth {
                    (i < ha).then(|| ca[hw - 1 - i])
                } else {
                    self.cell_load(ta, i)
                };
                if va.is_none() {
                    // before the history starts: plain panel (the picture shows through)
                    scr.put(hx + hw - 1 - i, y, ' ', NOPATCH, bg);
                    continue;
                }
                let up = heat(va);
                let lo = if gb >= n {
                    th.panel
                } else if smooth {
                    heat((i < hb).then(|| cb[hw - 1 - i]))
                } else {
                    heat(self.cell_load(&self.s.t.groups[gb], i))
                };
                scr.put(hx + hw - 1 - i, y, '▀', up, lo);
            }
            y += 1;
        }
        self.vals = ca;
        self.vals2 = cb;
        let th = &self.t;
        if axis {
            // minute ticks under the heatmap, "now" at the right
            let mut x = hx;
            while x < hx + hw {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
            let per_min = 60 / STRIP_SECS;
            let mut m = 1;
            while m * per_min + 3 < hw {
                let lab = f!("-{m}m");
                let tx = hx + hw - m * per_min;
                scr.put(tx, y, '╵', th.border, bg);
                scr.text(tx + 1, y, lab.s(), th.dim, bg, hx + hw);
                m += if hw / per_min > 8 { 2 } else { 1 };
            }
            scr.text(hx + hw - 3, y, "now", th.dim, bg, hx + hw);
        }
        if stats_w > 0 && stats {
            let sx = ir.x + ir.w - stats_w;
            for yy in 0..ir.h {
                scr.put(sx - 2, ir.y + yy, '┃', th.border, bg);
            }
            let mut v16 = std::mem::take(&mut self.tmp16);
            let mut v8 = std::mem::take(&mut self.tmp8);
            self.draw_stats(
                scr,
                self.selected,
                sx,
                ir.y,
                stats_w,
                ir.h,
                hw * STRIP_SECS,
                &mut v16,
                &mut v8,
            );
            self.tmp16 = v16;
            self.tmp8 = v8;
        }
    }

    fn draw_detail(&mut self, scr: &mut Screen, full: bool, body: bool) {
        let Some(r) = self.lay.det else { return };
        // the selected core's statistics change with history points, not every frame
        let sk = mixk(&[
            self.selected as u64,
            self.s.hist_n,
            self.s.peak.iter().fold(0.0f32, |a, &b| a.max(b)) as u64,
        ]);
        let stats = self.memo(SLOT_STATS, sk) || full || (body && !self.smooth());
        if self.lay.grid {
            self.draw_grid(scr, r, full, body, stats);
            return;
        }
        if !self.det_core && self.heat_fits(r) {
            if full || body {
                self.draw_heat(scr, r, full, stats);
            }
            return;
        }
        let gi = self.selected;
        if full {
            let name = f!("C{:0w$} ", gi, w = self.idw);
            let mut tl = F::new();
            for (i, &c) in self.s.t.groups[gi].iter().enumerate() {
                if i > 0 {
                    let _ = tl.write_str(" + ");
                }
                let _ = write!(tl, "t{:0w$}", self.s.t.cpus[c], w = self.idw);
            }
            self.panel(
                scr,
                r,
                &[(name.s(), self.t.title | BOLD), (tl.s(), self.t.mid)],
                "clock (top) · load (bottom)",
            );
        }
        let mut vals = std::mem::take(&mut self.vals);
        let mut v16 = std::mem::take(&mut self.tmp16);
        let mut v8 = std::mem::take(&mut self.tmp8);
        self.detail_body(
            scr,
            r,
            gi,
            &mut vals,
            &mut v16,
            &mut v8,
            full || body,
            stats,
        );
        self.vals = vals;
        self.tmp16 = v16;
        self.tmp8 = v8;
    }

    #[allow(clippy::too_many_arguments)]
    fn detail_body(
        &self,
        scr: &mut Screen,
        r: Rect,
        gi: usize,
        vals: &mut Vec<f32>,
        v16: &mut Vec<u16>,
        v8: &mut Vec<u8>,
        body: bool,
        stats: bool,
    ) {
        let threads = &self.s.t.groups[gi];
        let th = &self.t;
        let ir = r.inner();
        let (w, h) = (ir.w, ir.h);
        if h == 0 || w < 12 {
            return;
        }
        let bg = surf(S_PANEL);
        let stats_w = if w >= 110 { 30 } else { 0 };
        let n = threads.len().max(1);
        let gap = 3;
        let cw = if stats_w > 0 {
            ((w.saturating_sub(stats_w + gap * n)) / n).max(12)
        } else {
            ((w.saturating_sub(gap * (n - 1))) / n).max(12)
        };
        let lh = ((h.saturating_sub(1)) * 2 / 5).max(1);
        let fh = h.saturating_sub(1 + lh).max(1);
        let maxf = self.s.t.maxf;
        let mut x = ir.x;
        for (i, &c) in threads.iter().enumerate() {
            if x + cw > ir.x + w + 1 {
                break;
            }
            if i > 0 {
                for yy in 0..h {
                    scr.put(x - 2, ir.y + yy, '│', th.border, bg);
                }
            }
            let s = &self.s;
            let (f, u) = self.shown(c);
            let tn = f!("t{:0w$}", s.t.cpus[c], w = self.idw);
            let gz = ghz(f);
            let ld = num("", u * 100.0, 0, "%");
            let pk = ghz(s.peak[c]);
            let maxx = (x + cw).min(ir.x + w);
            Self::line(
                scr,
                x,
                ir.y,
                maxx,
                bg,
                &[
                    (tn.s(), th.fg | BOLD),
                    ("  ", 0),
                    (gz.s(), th.freq.at(f / maxf) | BOLD),
                    (" GHz", th.mid),
                    ("   load ", th.dim),
                    (ld.s(), th.load.at(u) | BOLD),
                    ("   peak ", th.dim),
                    (pk.s(), th.text),
                ],
            );
            let gw = cw.min(ir.x + w - x);
            if !body {
                x += cw + gap;
                continue;
            }
            self.hist_vals(vals, 2 * gw, 0.0, 1.0, Hist::Clock(&threads[i..i + 1]));
            let (mx, md) = (ghz(maxf), ghz(maxf / 2.0));
            // no "0" at the clock's foot: the load graph's "100%" sits right below it
            let fl: &[(f32, &str)] = if fh >= 5 {
                &[(1.0, mx.s()), (0.5, md.s())]
            } else {
                &[(1.0, mx.s())]
            };
            self.axis_graph(scr, x, ir.y + 1, gw, fh, vals, maxf, &th.freq, fl);
            self.hist_vals(vals, 2 * gw, 0.0, 1.0, Hist::Load(&threads[i..i + 1]));
            let ll: &[(f32, &str)] = if lh >= 2 {
                &[(1.0, "100%"), (0.0, "0")]
            } else {
                &[(0.5, "load")]
            };
            self.axis_graph(scr, x, ir.y + 1 + fh, gw, lh, vals, 1.0, &th.load, ll);
            x += cw + gap;
        }
        if stats_w > 0 && stats {
            let sx = ir.x + w - stats_w;
            for yy in 0..h {
                scr.put(sx - 2, ir.y + yy, '┃', th.border, bg);
            }
            self.draw_stats(
                scr,
                gi,
                sx,
                ir.y,
                stats_w,
                h,
                2 * cw.saturating_sub(5),
                v16,
                v8,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_stats(
        &self,
        scr: &mut Screen,
        gi: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        win: usize,
        v16: &mut Vec<u16>,
        v8: &mut Vec<u8>,
    ) {
        let s = &self.s;
        let th = &self.t;
        let bg = surf(S_PANEL);
        let threads = &s.t.groups[gi];
        let (mut fs, mut fn_, mut boost, mut us, mut un) = (0.0f64, 0usize, 0usize, 0.0f64, 0usize);
        for &c in threads {
            let have = s.hf[c].len().min(win);
            s.hf[c].tail(have, 0, v16);
            for &f in v16.iter() {
                fs += f as f64;
                fn_ += 1;
                if f as f32 >= s.t.boost && s.t.has_freq {
                    boost += 1;
                }
            }
            let have = s.hu[c].len().min(win);
            s.hu[c].tail(have, 0, v8);
            for &u in v8.iter() {
                us += u as f64 / 255.0;
                un += 1;
            }
        }
        let avg_f = if fn_ > 0 {
            (fs / fn_ as f64) as f32
        } else {
            0.0
        };
        let avg_u = if un > 0 { (us / un as f64) as f32 } else { 0.0 };
        let boost = if fn_ > 0 {
            boost as f32 / fn_ as f32
        } else {
            0.0
        };
        let peak = threads.iter().map(|&c| s.peak[c]).fold(0.0, f32::max);
        let maxf = s.t.maxf;
        let maxx = x + w;
        let mut yy = y;
        let name = f!("C{:0w$}", gi, w = self.idw);
        let ccd = if s.t.ndom > 1 {
            f!("  ·  CCD{}", s.t.ccd[gi])
        } else {
            f!("")
        };
        let thr = f!(
            "  ·  {} thread{}",
            threads.len(),
            if threads.len() == 1 { "" } else { "s" }
        );
        let mut line = |sp: &[(&str, u32)], yy: &mut usize| {
            if *yy < y + h {
                Self::line(scr, x, *yy, maxx, bg, sp);
                *yy += 1;
            }
        };
        line(
            &[
                (name.s(), th.accent | BOLD),
                (ccd.s(), th.mid),
                (thr.s(), th.mid),
            ],
            &mut yy,
        );
        let rank = s.t.rank[gi];
        if rank >= 0 {
            let pos = 1 + s.t.rank.iter().filter(|&&r| r > rank).count();
            let p = f!("#{pos}");
            let of = f!(" of {}", s.t.rank.len());
            line(
                &[
                    ("pref.   ", th.dim),
                    (p.s(), th.fg | BOLD),
                    (of.s(), th.mid),
                    (if pos == 1 { "  ◆ best" } else { "" }, th.warn),
                ],
                &mut yy,
            );
        }
        let (a, p) = (ghz(avg_f), ghz(peak));
        let b = num("", boost * 100.0, 0, "%");
        let l = num("", avg_u * 100.0, 0, "%");
        let wn = span(win);
        line(
            &[
                ("avg     ", th.dim),
                (a.s(), th.freq.at(avg_f / maxf)),
                (" GHz", th.mid),
            ],
            &mut yy,
        );
        line(
            &[
                ("peak    ", th.dim),
                (p.s(), th.freq.at(peak / maxf)),
                (" GHz", th.mid),
            ],
            &mut yy,
        );
        line(
            &[
                ("boost   ", th.dim),
                (b.s(), th.fg),
                (" of the time", th.mid),
            ],
            &mut yy,
        );
        line(
            &[
                ("load    ", th.dim),
                (l.s(), th.load.at(avg_u)),
                (" average", th.mid),
            ],
            &mut yy,
        );
        line(&[("window  ", th.dim), (wn.s(), th.text)], &mut yy);
    }

    /// Page tabs at the footer's left, the current one highlighted: "1 cpu 2 mem
    /// 3 net 4 proc" (narrow screens: "1 2 3 4"). Clickable (keys 1-4). Returns the x
    /// after them.
    fn draw_tabs(&mut self, scr: &mut Screen, y: usize) -> usize {
        let w = self.w;
        let long = w >= 72;
        let th = &self.t;
        let bar = surf(S_BAR);
        let mut x = 0;
        for i in 0..NPAGES {
            let lab = page_name(i).0;
            let x0 = x;
            let d = (b'1' + i as u8) as char;
            let (dc, lc, bg) = if i == self.page {
                (th.bg | BOLD, th.bg | BOLD, th.accent)
            } else {
                (th.accent | BOLD, th.mid, bar)
            };
            scr.put(x, y, ' ', 0, bg);
            scr.put(x + 1, y, d, dc, bg);
            x = scr.text(x + 2, y, " ", 0, bg, w);
            if long {
                x = scr.text(x, y, lab, lc, bg, w);
                x = scr.text(x, y, " ", 0, bg, w);
            }
            self.zones.push((x0, x, d));
        }
        x + 1
    }

    fn draw_keys(&mut self, scr: &mut Screen) {
        let w = self.w;
        let y = self.h - 1;
        let bar = surf(S_BAR);
        scr.fill(
            0,
            y,
            w,
            1,
            Cell {
                ch: ' ',
                fg: 0,
                bg: bar,
            },
        );
        // page tabs first (1 cpu 2 mem 3 net 4 proc), then the page's keys
        self.zones.clear();
        let tx = self.draw_tabs(scr, y);
        let ntab = self.zones.len();
        let th = &self.t;
        // (key, long, short, label)
        let cpu_keys: [(char, &str, Option<&str>, &str); 11] = [
            ('q', "quit", Some("quit"), "q"),
            (' ', "pause", Some("pause"), "spc"),
            ('s', "speed −/+", Some("speed"), "s"),
            ('m', "memory", Some("mem"), "m"),
            ('o', "sort", Some("sort"), "o"),
            ('c', "view", Some("view"), "c"),
            ('g', "graphs", Some("graphs"), "g"),
            ('d', "detail", Some("detail"), "d"),
            ('r', "reset peaks", Some("peaks"), "r"),
            ('t', "theme", Some("theme"), "t"),
            ('\0', "select", None, "←↑↓→"),
        ];
        let mem_keys: [(char, &str, Option<&str>, &str); 5] = [
            cpu_keys[0],
            cpu_keys[1],
            cpu_keys[2],
            ('m', "cpu view", Some("cpu"), "m"),
            ('t', "theme", Some("theme"), "t"),
        ];
        // view 4 (procui.rs); keys without a short text drop out first
        let proc_keys: [(char, &str, Option<&str>, &str); 13] = [
            ('q', "quit", Some("quit"), "q"),
            (' ', "pause", None, "spc"),
            ('s', "speed", Some("speed"), "s"),
            ('o', "sort", Some("sort"), "o"),
            ('r', "reverse", Some("rev"), "r"),
            ('/', "filter", Some("filter"), "/"),
            ('T', "tree", Some("tree"), "T"),
            ('G', "group", Some("group"), "G"),
            ('k', "kill", Some("kill"), "k"),
            ('n', "nice", Some("nice"), "n"),
            ('I', "cpu % scale", None, "I"),
            ('K', "kernel threads", None, "K"),
            ('t', "theme", Some("theme"), "t"),
        ];
        let keys: &[(char, &str, Option<&str>, &str)] = match self.page {
            PG_MEM => &mem_keys,
            PG_NET => &mem_keys[..3],
            PG_PROC => &proc_keys,
            _ => &cpu_keys,
        };
        let pv = &self.pv;
        let state = |k: char| -> &str {
            match k {
                's' => SPEEDS[self.speed].0,
                'o' if self.page == PG_PROC => psort_name(pv.sort),
                'T' if pv.tree => "on",
                'G' if pv.group && !pv.tree => "on",
                'o' => SORTS[self.sort],
                'c' => VIEWS[self.view],
                'd' if !self.show_det => "off",
                'd' if self.det_core => "core",
                't' => &self.t.label,
                _ => "",
            }
        };
        let w = w.saturating_sub(tx);
        // variants, most detailed first: (desc 0 long / 1 short / 2 none, pad, sep, theme name)
        for (dv, pad, sep, tn) in [
            (0, true, 2, true),
            (1, true, 2, true),
            (1, true, 1, true),
            (1, false, 1, true),
            (1, false, 1, false),
            (2, false, 1, false),
        ] {
            let st = |k: char| if k == 't' && !tn { "" } else { state(k) };
            let mut len = 0;
            for &(k, long, short, lab) in keys {
                let d = match dv {
                    0 => long,
                    1 => short.unwrap_or(""),
                    _ => "",
                };
                if dv > 0 && short.is_none() {
                    continue;
                }
                len += lab.chars().count() + if pad { 2 } else { 0 };
                if !d.is_empty() {
                    len += 1 + d.chars().count();
                }
                let sv = st(k);
                if !sv.is_empty() {
                    len += 1 + sv.chars().count();
                }
                len += sep;
            }
            if len.saturating_sub(sep) > w && dv < 2 {
                continue;
            }
            self.zones.truncate(ntab);
            let w = w + tx;
            let mut x = tx;
            for &(k, long, short, lab) in keys {
                let d = match dv {
                    0 => long,
                    1 => short.unwrap_or(""),
                    _ => "",
                };
                if dv > 0 && short.is_none() {
                    continue;
                }
                let x0 = x;
                if pad {
                    x = scr.text(x, y, " ", th.accent, surf(S_CHIP), w);
                }
                x = scr.text(x, y, lab, th.accent | BOLD, surf(S_CHIP), w);
                if pad {
                    x = scr.text(x, y, " ", th.accent, surf(S_CHIP), w);
                }
                if !d.is_empty() {
                    x = scr.text(x, y, " ", 0, bar, w);
                    x = scr.text(x, y, d, th.mid, bar, w);
                }
                let sv = st(k);
                if !sv.is_empty() {
                    x = scr.text(x, y, " ", 0, bar, w);
                    x = scr.text(x, y, sv, th.fg, bar, w);
                }
                if k != '\0' {
                    self.zones.push((x0, x, k));
                }
                x += sep;
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake 16-core / 32-thread, 2-CCD machine (or any shape).
    fn app(cores: usize, smt: usize, ccds: usize) -> App {
        let n = cores * smt;
        let mut s = Sampler::new_for_test(&(0..n).collect::<Vec<_>>());
        s.t.groups = (0..cores)
            .map(|c| (0..smt).map(|k| c + k * cores).collect())
            .collect();
        s.t.ccd = (0..cores).map(|c| c * ccds / cores).collect();
        s.t.ndom = ccds;
        s.t.rank = vec![-1; cores];
        s.t.maxf = 5760.0;
        s.t.minf = 620.0;
        s.t.has_freq = true;
        for i in 0..n {
            s.f[i] = 620.0 + (i * 157 % 5000) as f32;
            s.u[i] = (i * 37 % 100) as f32 / 100.0;
        }
        for _ in 0..200 {
            s.sample_fake();
        }
        let mut a = App::new(s, crate::theme::load("default").unwrap());
        a.m = crate::mem::Mem::fake(1, 3, 12);
        a
    }

    fn check(a: &App, w: usize, h: usize) {
        check_mem(a, w, h);
        if a.page == 1 {
            return;
        }
        let l = &a.lay;
        let w = a.cw;
        let inside = |r: Rect| r.x + r.w <= w && r.y >= 1 && r.y + r.h < h;
        assert!(inside(l.cores), "{w}x{h} cores {:?}", l.cores);
        if let Some(t) = l.top {
            assert!(inside(t) && t.y + t.h <= l.cores.y, "{w}x{h} top");
            assert!(l.g_load.w + l.g_freq.w + l.sensors.w == w);
        }
        if let Some(d) = l.det {
            assert!(inside(d) && d.y >= l.cores.y + l.cores.h, "{w}x{h} detail");
        }
        let ir = l.cores.inner();
        for &(cx, cw) in &l.card_x {
            assert!(cx + cw <= ir.w, "{w}x{h} card column overflows");
        }
        let step = l.rh + l.gap as usize;
        let used = l.header as usize + l.vis_rows.min(l.nrows) * step - l.gap as usize;
        assert!(used <= ir.h, "{w}x{h} rows overflow: {used} > {}", ir.h);
    }

    /// Memory panels: on screen, beside (not over) the CPU view, not overlapping,
    /// tall enough for a border and a row.
    fn check_mem(a: &App, w: usize, h: usize) {
        let rs: Vec<Rect> = a.ml.rects().into_iter().filter(|r| r.w > 0).collect();
        for (i, r) in rs.iter().enumerate() {
            assert!(
                r.x + r.w <= w && r.y >= 1 && r.y + r.h < h,
                "{w}x{h} mem {r:?}"
            );
            assert!(r.h >= 3 && r.w >= 20, "{w}x{h} mem panel too small {r:?}");
            if a.page == 0 {
                assert!(r.x >= a.cw, "{w}x{h} mem over the CPU view");
            }
            for q in &rs[i + 1..] {
                let apart =
                    r.x + r.w <= q.x || q.x + q.w <= r.x || r.y + r.h <= q.y || q.y + q.h <= r.y;
                assert!(apart, "{w}x{h} overlap {r:?} {q:?}");
            }
        }
    }

    #[test]
    fn memory_page_and_column_fit_at_all_sizes() {
        for (nd, nfs) in [(1, 3), (0, 0), (3, 8), (2, 1)] {
            for w in (20..=420).step_by(41) {
                for h in (6..=110).step_by(13) {
                    for page in [0, 1] {
                        let mut a = app(16, 2, 2);
                        a.m = crate::mem::Mem::fake(nd, nfs, 24);
                        a.page = page;
                        a.first_size(w, h);
                        if h >= 8 && w >= 30 {
                            check(&a, w, h);
                        }
                        let mut scr = Screen::new(w, h);
                        a.draw(&mut scr, true);
                        a.draw(&mut scr, false);
                        a.draw_bars(&mut scr);
                    }
                }
            }
        }
        // the memory page shows every panel at the usual sizes
        for (w, h) in [(128, 45), (80, 24), (172, 40), (415, 105)] {
            let mut a = app(16, 2, 2);
            a.page = 1;
            a.first_size(w, h);
            let l = a.ml;
            assert!(
                l.mem.w > 0 && l.disk.w > 0 && l.fs.w > 0 && l.procs.w > 0,
                "{w}x{h} {l:?}"
            );
        }
        // 40x12: memory, disk and storage
        let mut a = app(16, 2, 2);
        a.page = 1;
        a.first_size(40, 12);
        assert!(a.ml.mem.w > 0 && a.ml.disk.w > 0 && a.ml.fs.w > 0);
        // the column beside the CPU view from 1080p maximised up, all cores still visible
        for (w, h, side) in [
            (128, 45, false),
            (172, 40, false),
            (205, 50, true),
            (275, 70, true),
            (415, 105, true),
        ] {
            let mut a = app(16, 2, 2);
            a.first_size(w, h);
            assert_eq!(a.mem_visible(), side, "{w}x{h}");
            assert!(a.lay.fits && a.lay.nrows <= a.lay.vis_rows, "{w}x{h}");
            if side {
                assert!(
                    a.lay.top.is_some() && a.lay.det.is_some() && a.ml.procs.w > 0,
                    "{w}x{h}"
                );
            }
        }
    }

    #[test]
    fn memory_values_glide_in_smooth_mode() {
        let mut a = app(16, 2, 2);
        a.page = 1;
        a.first_size(128, 45);
        a.bars_snap();
        let g = (1u64 << 30) as f32;
        assert_eq!(a.ma.used.x, 5.0 * g);
        a.speed = SMOOTH;
        a.m.mi.avail = 20 * (1 << 30); // used 5 → 10 GiB
        a.m.disks[0].rd = 100e6;
        a.retarget();
        let dt = 1.0 / crate::anim::FPS as f32;
        let (mut prev, mut shown, mut t) = (a.ma.used.x, vec![a.ma.used.n], 0.0);
        let mut scr = Screen::new(128, 45);
        while a.anim_step(dt) | a.anim_numbers(dt) {
            assert!(
                a.ma.used.x >= prev && a.ma.used.x <= 10.0 * g * 1.0001,
                "monotone, no overshoot"
            );
            prev = a.ma.used.x;
            if shown.last() != Some(&a.ma.used.n) {
                shown.push(a.ma.used.n);
            }
            a.anim = true;
            a.frame += 1;
            a.draw(&mut scr, false);
            t += dt;
            assert!(t < 4.0);
        }
        assert_eq!(a.ma.used.x, 10.0 * g);
        assert_eq!(a.ma.used.n, 10.0 * g);
        // the digits counted through the values in between
        assert!(
            shown.len() > 10 && shown.windows(2).all(|w| w[1] > w[0]),
            "{shown:?}"
        );
        assert!(a.ma.disk[0][0].x == 100e6);
    }

    #[test]
    fn all_cores_visible_at_the_three_sizes() {
        for (w, h, compact) in [(172, 40, false), (120, 30, false), (80, 24, true)] {
            let mut a = app(16, 2, 2);
            a.first_size(w, h);
            check(&a, w, h);
            assert!(a.lay.fits, "{w}x{h}: cores must fit without scrolling");
            assert_eq!(a.lay.compact, compact, "{w}x{h} view");
            assert!(
                a.lay.nrows <= a.lay.vis_rows,
                "{w}x{h}: {} rows, {} visible",
                a.lay.nrows,
                a.lay.vis_rows
            );
            assert_eq!(a.lay.cols * a.lay.nrows >= 16, true);
        }
        // 172x40 = graphs + cores + detail (one CCD per column); the detail area takes a
        // row from the graphs so the all-core heatmap (8 rows + borders) fits
        let mut a = app(16, 2, 2);
        a.first_size(172, 40);
        assert!(a.lay.top.is_some() && a.lay.det.is_some());
        assert_eq!(
            (
                a.lay.cols,
                a.lay.nrows,
                a.lay.top.unwrap().h,
                a.lay.cores.h,
                a.lay.det.unwrap().h
            ),
            (2, 8, 9, 19, 10)
        );
        assert!(a.heat_fits(a.lay.det.unwrap()));
    }

    #[test]
    fn layout_and_drawing_at_many_sizes() {
        for (cores, smt, ccds) in [
            (16, 2, 2),
            (8, 1, 1),
            (4, 2, 1),
            (64, 2, 8),
            (1, 1, 1),
            (12, 2, 2),
        ] {
            for w in (20..=260).step_by(13) {
                for h in (6..=80).step_by(5) {
                    let mut a = app(cores, smt, ccds);
                    a.first_size(w, h);
                    if h >= 8 && w >= 30 {
                        check(&a, w, h);
                    }
                    if a.lay.fits {
                        assert!(a.lay.nrows <= a.lay.vis_rows, "{cores}c {w}x{h}");
                    }
                    let mut scr = Screen::new(w, h);
                    a.draw(&mut scr, true);
                    a.draw(&mut scr, false);
                    // toggles keep working
                    a.show_top = !a.show_top;
                    a.fit(w, h);
                    a.draw(&mut scr, true);
                    a.menu = Some(crate::menu::Menu::new("default"));
                    a.draw(&mut scr, true);
                }
            }
        }
        // 16 cores fit without scrolling down to 40x12 (micro cards)
        for (w, h) in [
            (40, 12),
            (50, 16),
            (60, 20),
            (45, 12),
            (90, 50),
            (200, 30),
            (415, 105),
        ] {
            let mut a = app(16, 2, 2);
            a.first_size(w, h);
            check(&a, w, h);
            assert!(a.lay.fits && a.lay.nrows <= a.lay.vis_rows, "{w}x{h}");
        }
        for w in [80, 100, 120, 140, 172, 200, 250] {
            for h in [24, 30, 40, 50, 60] {
                let mut a = app(16, 2, 2);
                a.first_size(w, h);
                assert!(a.lay.fits && a.lay.nrows <= a.lay.vis_rows, "{w}x{h}");
            }
        }
    }

    #[test]
    fn navigation_and_clicks() {
        let mut a = app(16, 2, 2);
        a.first_size(172, 40);
        a.selected = 0;
        a.move_sel(0, 1);
        assert_eq!(a.selected, 1);
        a.move_sel(1, 0);
        assert_eq!(a.selected, 9); // columns run top to bottom: C08.. in column 2
        a.move_sel(1, 0);
        assert_eq!(a.selected, 9);
        let mut scr = Screen::new(172, 40);
        a.draw(&mut scr, true);
        let ir = a.lay.cores.inner();
        let g = a.card_at(ir.x + 5, ir.y + 1 + 2 * a.lay.rh);
        assert_eq!(g, Some(2));
        // page tabs first (1 cpu 2 mem 3 net 4 proc), then the keys
        assert_eq!(a.key_at(1, 39), Some('1'));
        for k in ['4', 'q', 't'] {
            let z = a.zones.iter().find(|z| z.2 == k).unwrap();
            assert_eq!(a.key_at(z.0, 39), Some(k));
        }
    }

    #[test]
    fn smooth_bars_and_digits_follow_samples_and_come_to_rest() {
        let mut a = app(16, 2, 2);
        a.fit(172, 40);
        a.speed = SMOOTH;
        let n = a.bf.len();
        a.bars_snap(); // from rest at 0
        for i in 0..n {
            a.s.fe[i] = 5000.0;
            a.s.ur[i] = 0.3;
        }
        a.retarget();
        let dt = 1.0 / crate::anim::FPS as f32;
        let (mut t, mut prev, mut digits) = (0.0, 0.0, vec![a.shown(0).1]);
        while a.anim_step(dt) {
            a.anim_numbers(dt);
            assert!(
                a.bu[0] >= prev && a.bu[0] <= 0.3 + 1e-6,
                "monotone, no overshoot"
            );
            prev = a.bu[0];
            if digits.last() != Some(&a.shown(0).1) {
                digits.push(a.shown(0).1);
            }
            t += dt;
            assert!(t < 3.0);
        }
        a.anim_numbers(dt);
        assert!(a.bu.iter().all(|&u| u == 0.3));
        assert!(a.bf.iter().all(|&f| f == 5000.0 / 5760.0));
        // the digits counted up through the values in between, ending on the target
        assert!(digits.len() > 10 && digits.windows(2).all(|w| w[1] > w[0]));
        assert!((a.shown(0).1 - 0.3).abs() < 1e-6 && a.shown(0).0 == 5000.0);
        // the numbers drawn and the totals follow the bars
        assert!((a.totals().0 - 0.3).abs() < 1e-6);
        let mut scr = Screen::new(172, 40);
        a.draw(&mut scr, true);
        // other speeds: bars jump to the sample
        a.speed = 0;
        a.s.ur[0] = 0.5;
        a.bars_snap();
        assert_eq!(a.bu[0], 0.5);
    }

    #[test]
    fn smooth_history_scrolls_with_the_live_value_at_its_head() {
        let mut a = app(16, 2, 2);
        a.first_size(172, 40);
        let mut v = Vec::new();
        // other speeds: the points as they are
        a.hist_vals(&mut v, 4, 0.0, 1.0, Hist::Tot);
        assert_eq!(v, vec![0.3; 4]);
        a.speed = SMOOTH;
        a.bars_snap();
        a.bu.iter_mut().for_each(|u| *u = 0.9);
        a.phase = 0.5;
        a.hist_vals(&mut v, 4, 0.0, 1.0, Hist::Tot);
        assert!((v[3] - 0.9).abs() < 1e-6, "{v:?}"); // age 0: live
        assert!((v[2] - 0.3).abs() < 1e-6, "{v:?}"); // age 1: newest point
                                                     // strips / heatmap cells: means over 2 s
        let thr = a.s.t.groups[0].clone();
        let mut c = Vec::new();
        let have = a.cells_smooth(&mut c, 6, &thr);
        assert_eq!((c.len(), have), (6, 6));
        let mut scr = Screen::new(172, 40);
        a.draw(&mut scr, true);
        a.anim = true;
        a.frame += 1;
        a.draw(&mut scr, false);
    }

    #[test]
    fn bars_have_sixteenth_cell_resolution() {
        assert_eq!(fill16(0.0, 10), 0);
        assert_eq!(fill16(1.0, 10), 160);
        assert_eq!(fill16(0.5 + 1.0 / 320.0, 10), 81);
        assert_eq!(fill16(2.0, 10), 160);
        let a = app(4, 1, 1);
        let mut scr = Screen::new(12, 1);
        a.meter(&mut scr, 0, 0, 0.30625, 10, &a.t.load, surf(S_PANEL), LOAD); // 3 1/16 cells
        let cell = |i: usize| scr.buf[i].fg;
        assert!((0..3).all(|i| cell(i) & (TRACK | BLEND) == 0));
        assert_eq!((cell(3) & BLEND) >> BLEND_SHIFT, 1, "edge cell covers 1/16");
        assert!((4..10).all(|i| cell(i) & TRACK != 0));
    }

    #[test]
    fn formatting() {
        assert_eq!(ghz(5530.0).s(), "5.53");
        assert_eq!(ghz(620.0).s(), "0.62");
        assert_eq!(ghz(10004.0).s(), "10.00");
        assert_eq!(num(" ", 7.4, 3, "%").s(), "   7%");
        assert_eq!(num("", 100.0, 3, "").s(), "100");
        assert_eq!(span(108).s(), "1m 48s");
        assert_eq!(span(54).s(), "54s");
        use crate::sys::pf;
        assert_eq!(pf(" 4.21\n"), Some(4.21));
        assert_eq!(pf("0.6"), Some(0.6));
        assert_eq!(pf("-2"), Some(-2.0));
        assert_eq!((pf("x"), pf(""), pf("1.2.3")), (None, None, None));
        assert_eq!(ghz(4.21 * 1000.0).s(), "4.21");
    }
}

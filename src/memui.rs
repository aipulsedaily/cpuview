//! Memory & Storage: RAM, swap, disk activity, filesystems and the programs using the
//! most memory. Shown as its own page (`m`) or, on big screens, as a column beside the
//! CPU view.
//!
//! Written for a reader who isn't a Linux expert, in the CPU view's row grammar:
//! every panel opens with one headline answer in its title ("RAM 17% in use · 5.50 of
//! 32.2 GB · plenty free"), rows read label → value → bar → %, with a short plain hint
//! where there is room, and colours mean health (green fine, amber watch, red act).
//! "How full now" is always a half-block bar; "over time" is always a chart with a
//! y-axis in units, "… ago · now" time labels in the panel border and the current
//! value at its right edge. Sizes are decimal (1 GB = 10⁹ bytes, as `df -H` and disk
//! makers count).

use super::*;
use crate::anim::{Val, W_CLOCK, W_LOAD, W_TEMP};
use crate::color::mix_lin;
use crate::f;
use crate::mem::slow_temp;

/// Memory-area panels (`w == 0`: not shown).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct MemLay {
    pub mem: Rect,
    pub disk: Rect,
    pub fs: Rect,
    pub procs: Rect,
    /// disk panel inner width in cells
    pub gw: usize,
}

impl MemLay {
    pub fn rects(&self) -> [Rect; 4] {
        [self.mem, self.disk, self.fs, self.procs]
    }
}

/// A row of the programs panel: its memory, its row position (slides when the order
/// changes) and its opacity (fades in and out).
#[derive(Clone, Copy, Default, Debug)]
pub struct PRow {
    pub key: u64,
    pub mem: Val,
    pub pos: Val,
    pub fade: Val,
    /// index into `Mem::groups` (programs) or `Mem::procs` (processes); usize::MAX:
    /// leaving
    pub src: usize,
}

/// Animated values of the memory area (see `anim::Val`).
#[derive(Default)]
pub struct MA {
    pub(super) used: Val,
    pub(super) cache: Val,
    pub(super) avail: Val,
    pub(super) swap: Val,
    /// swap in + out, bytes/s
    pub(super) swio: Val,
    pub(super) psi: Val,
    pub(super) dimm: Vec<Val>,
    /// per disk: read, write (bytes/s), busy (0..1), temperature
    pub(super) disk: Vec<[Val; 4]>,
    pub(super) fs: Vec<Val>,
    pub(super) rows: Vec<PRow>,
    /// RAM chart's newest point, held to whole dots (no flicker at a dot's edge)
    pub(super) head: f32,
    /// disk chart's newest column (read, write), held to whole eighths, and when it
    /// last moved (smooth: it moves at most every 150 ms, so a one-sample burst can't
    /// blink a bar on and off)
    pub(super) dhead: std::cell::Cell<(f32, f32, i64)>,
    /// programs panel: one row per process instead of one per program
    pub split: bool,
}

pub(super) const D_RD: usize = 0;
pub(super) const D_WR: usize = 1;
const D_BUSY: usize = 2;
const D_TEMP: usize = 3;

/// memo slots (App::mcache)
const K_MEM: usize = 0;
const K_MGRAPH: usize = 1;
const K_DISK: usize = 2;
const K_DGRAPH: usize = 3;
const K_FS: usize = 4;
const K_PROC: usize = 5;
const K_MTITLE: usize = 6;
const K_DTITLE: usize = 7;

// ── health thresholds ─────────────────────────────────────────────────────────

/// RAM in use (fraction): amber from 80 %, red from 90 %.
pub const RAM_WARN: f32 = 0.80;
pub const RAM_CRIT: f32 = 0.90;
/// Swap traffic (in + out, bytes/s): amber from 1 MB/s, red from 20 MB/s.
pub const SWAP_WARN: f32 = 1e6;
pub const SWAP_CRIT: f32 = 20e6;
/// Memory pressure (% of time some program waited for RAM, last 10 s).
pub const PSI_WARN: f32 = 5.0;
pub const PSI_CRIT: f32 = 20.0;
/// Filesystem use %.
pub const FS_WARN: f32 = 90.0;
pub const FS_CRIT: f32 = 95.0;
/// Drive temperature (°C): NVMe drives throttle from ~80 °C.
pub const DISK_WARN: f32 = 70.0;
pub const DISK_CRIT: f32 = 80.0;
/// DIMM temperature (°C): DDR5 modules are rated to 85 °C.
pub const DIMM_WARN: f32 = 70.0;
pub const DIMM_CRIT: f32 = 85.0;

/// 0 fine, 1 watch, 2 act.
pub fn level(v: f32, warn: f32, crit: f32) -> u8 {
    if v >= crit {
        2
    } else if v >= warn {
        1
    } else {
        0
    }
}

/// Below this a disk counts as idle (bytes/s).
pub const IDLE_BPS: f32 = 50e3;

// ── formatting (decimal units, no float formatting) ─────────────────────────────

const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];

/// (value in its unit, unit index): below 999.5 of the unit.
fn unit_of(v: f32) -> (f32, usize) {
    let (mut v, mut k) = (v.max(0.0), 0);
    while v >= 999.5 && k < 5 {
        v /= 1000.0;
        k += 1;
    }
    (v, k)
}

/// 3 significant digits of `v` (already in its unit; `whole`: no decimals).
fn sig3(f: &mut F, v: f32, whole: bool) {
    let c = |m: f32| (v * m).round() as u32;
    if whole || v >= 99.95 {
        f.int(c(1.0), 0);
    } else if v >= 9.995 {
        f.int(c(10.0) / 10, 0);
        f.push(b'.');
        f.push(b'0' + (c(10.0) % 10) as u8);
    } else {
        f.int(c(100.0) / 100, 0);
        f.push(b'.');
        f.push(b'0' + (c(100.0) / 10 % 10) as u8);
        f.push(b'0' + (c(100.0) % 10) as u8);
    }
}

/// Size with 3 significant digits and a decimal unit: "4.65 GB", "229 MB" (short:
/// "4.65G").
pub fn bsize(v: f32, short: bool) -> F {
    let (v, k) = unit_of(v);
    let mut f = F::new();
    sig3(&mut f, v, k == 0);
    if short {
        f.push(b"BkMGTP"[k]);
    } else {
        f.push(b' ');
        f.str(UNITS[k]);
    }
    f
}

/// "5.50 of 32.2 GB" (one unit when both share it), else "512 MB of 32.2 GB".
pub fn of_size(a: f32, b: f32) -> F {
    let (ua, ka) = unit_of(a);
    let mut f = F::new();
    if ka == unit_of(b).1 && ka > 0 {
        sig3(&mut f, ua, false);
    } else {
        f.str(bsize(a, false).s());
    }
    f.str(" of ");
    f.str(bsize(b, false).s());
    f
}

/// The resolution `bsize` shows `v` with (bytes).
pub fn bstep(v: f32) -> f32 {
    let (u, k) = unit_of(v);
    let s = if k == 0 || u >= 99.95 {
        1.0
    } else if u >= 9.995 {
        0.1
    } else {
        0.01
    };
    s * 1000f32.powi(k as i32)
}

/// A transfer rate, always in MB/s so every rate on the page (and the chart's axis)
/// compares at a glance: "0 MB/s", "0.4 MB/s", "18.8 MB/s", "113 MB/s", "2540 MB/s".
pub fn rate(v: f32, _short: bool) -> F {
    let m = v.max(0.0) / 1e6;
    let mut f = F::new();
    if m < 0.05 {
        f.push(b'0');
    } else if m < 9.95 {
        let t = (m * 10.0).round() as u32;
        f.int(t / 10, 0);
        f.push(b'.');
        f.push(b'0' + (t % 10) as u8);
    } else {
        f.int(m.round() as u32, 0);
    }
    f.str(" MB/s");
    f
}

/// An axis label for a round rate: "3000 MB/s", "1500 MB/s", "5 MB/s".
pub fn rate_label(v: f32) -> F {
    let t = (v / 1e5).round() as u32;
    let mut f = F::new();
    f.int(t / 10, 0);
    if t % 10 != 0 {
        f.push(b'.');
        f.push(b'0' + (t % 10) as u8);
    }
    f.str(" MB/s");
    f
}

/// Chart scale for a rate peak: 1, 2, 3 or 5 × 10ⁿ bytes/s, 10 MB/s at least (so a
/// quiet disk stays a flat line instead of magnified noise).
pub fn nice_rate(v: f32) -> f32 {
    let mut p = 10e6f32;
    loop {
        for m in [1.0f32, 2.0, 3.0, 5.0] {
            if m * p >= v || p >= 1e15 {
                return m * p;
            }
        }
        p *= 10.0;
    }
}

/// Right-aligned to `w` characters.
pub fn rpad(f: &F, w: usize) -> F {
    let mut o = F::new();
    for _ in f.s().chars().count()..w {
        o.push(b' ');
    }
    o.str(f.s());
    o
}

/// "4.1%": a fraction as a percentage with one decimal.
pub fn pct1(x: f32) -> F {
    let c = (x * 1000.0).max(0.0).round() as u32;
    let mut f = F::new();
    f.int(c / 10, 0);
    f.push(b'.');
    f.push(b'0' + (c % 10) as u8);
    f.push(b'%');
    f
}

/// "58 s ago", "2 min ago", "1 min 30 s ago".
pub fn ago(secs: usize) -> F {
    let mut f = dur(secs);
    f.str(" ago");
    f
}

/// "2 min", "1 min 30 s", "45 s".
pub fn dur(secs: usize) -> F {
    let (m, s) = (secs / 60, secs % 60);
    if m == 0 {
        f!("{s} s")
    } else if s == 0 || m >= 5 {
        f!("{m} min")
    } else {
        f!("{m} min {s} s")
    }
}

/// Label column width of the rows ("available ").
pub(super) const LW: usize = 10;
/// Value column width ("18.8 MB/s").
pub(super) const VW: usize = 9;

/// A stacked rate column for the disk chart.
#[derive(Clone, Copy, Default)]
pub(super) struct Col {
    pub(super) rd: f32,
    pub(super) wr: f32,
}

impl App {
    /// The memory area is on screen (its own page, or beside the CPU view).
    pub fn mem_visible(&self) -> bool {
        self.ml.mem.w > 0
    }

    pub fn procs_visible(&self) -> bool {
        self.ml.procs.w > 0
    }

    /// Programs panel: one row per program ⇄ one per process.
    pub fn toggle_split(&mut self) {
        self.ma.split = !self.ma.split;
        self.ma.rows.clear();
        self.mem_retarget(false);
    }

    pub(crate) fn hcol(&self, l: u8) -> u32 {
        match l {
            0 => self.t.good,
            1 => self.t.warn,
            _ => self.t.crit,
        }
    }

    // ── layout ─────────────────────────────────────────────────────────────

    /// RAM panel: statistics rows (in use, available, cache, swap, waiting, temp).
    fn ram_stats(&self) -> usize {
        4 + self.m.has_psi as usize + !self.m.dimms.is_empty() as usize
    }

    /// Disk panel: statistics rows (reading, writing, busy, temperature / drive; a
    /// name row per disk for two disks).
    fn disk_stats(&self) -> usize {
        match self.m.disks.len() {
            0 => 0,
            1 => 4,
            2 => 10,
            // more disks: one line each (and the chart of all of them)
            n => n,
        }
    }

    /// Panels in `r`: two columns (memory over storage | disks over programs) or one,
    /// by priority: memory, disks, storage, programs; then statistics, then charts
    /// (disk first) and more programs.
    pub fn mem_layout(&self, r: Rect, two: bool) -> MemLay {
        let m = &self.m;
        let nd = m.disks.len();
        let nfs = m.fs.len();
        let np = crate::mem::NGROUP;
        let mut l = MemLay::default();
        let at = |x: usize, y: usize, w: usize, h: usize| Rect { x, y, w, h };
        if two {
            let (lw, rw) = (r.w / 2, r.w - r.w / 2);
            let top_min = (2 + self.ram_stats()).max(2 + self.disk_stats()) + 4;
            let fs_need = 2 + 2 * nfs.max(1);
            let b = fs_need
                .max((r.h * 36 / 100).min(4 + np))
                .min(r.h.saturating_sub(top_min))
                .max((2 + nfs.max(1)).min(r.h / 2));
            let t = r.h - b;
            l.mem = at(r.x, r.y, lw, t);
            l.fs = at(r.x, r.y + t, lw, b);
            if nd > 0 {
                l.disk = at(r.x + lw, r.y, rw, t);
                l.procs = at(r.x + lw, r.y + t, rw, b);
            } else {
                l.procs = at(r.x + lw, r.y, rw, r.h);
            }
            if nfs == 0 {
                l.mem.h = r.h;
                l.fs = Rect::default();
            }
            l.gw = rw - 4;
        } else {
            let iw = r.w.saturating_sub(4);
            // (min, statistics, chart) heights
            let ds = self.disk_stats();
            let pans: [(usize, usize, usize); 4] = [
                (3, 2 + self.ram_stats(), 7),
                (
                    if nd > 0 { 2 + nd } else { 0 },
                    if nd > 0 { 2 + ds } else { 0 },
                    if nd > 0 { 8 } else { 0 },
                ),
                (
                    // (the root filesystem at least; "+ N more" says the rest)
                    if nfs > 0 { 3 } else { 0 },
                    if nfs > 0 { 2 + 2 * nfs } else { 0 },
                    0,
                ),
                (if r.w >= 34 { 5 } else { 0 }, 4 + np, 0),
            ];
            let mut hs = [0usize; 4];
            let mut left = r.h;
            for (k, p) in pans.iter().enumerate() {
                // (storage may show fewer filesystems than it has)
                let need = if k == 2 { p.0.min(left) } else { p.0 };
                if need > 0 && need <= left && (k != 2 || need >= 3) {
                    hs[k] = need;
                    left -= need;
                }
            }
            if hs[0] == 0 && r.h >= 3 {
                hs[0] = r.h.min(3);
                left = r.h - hs[0];
            }
            // statistics: disk rows all or one line per disk; storage one line each
            for k in [1, 0, 2] {
                if hs[k] == 0 {
                    continue;
                }
                let want = if k == 2 { 2 + nfs } else { pans[k].1 };
                if k == 1 && want - hs[1] > left {
                    continue;
                }
                let g = want.saturating_sub(hs[k]).min(left);
                hs[k] += g;
                left -= g;
            }
            // charts: the disk's first, then RAM's; a one-line trend where a chart
            // doesn't fit
            let mut charts = 0;
            for k in [1, 0] {
                if hs[k] > 0 && hs[k] >= pans[k].1 && left >= pans[k].2 {
                    hs[k] += pans[k].2;
                    left -= pans[k].2;
                    charts += 1;
                }
            }
            for k in [1, 0] {
                if hs[k] > 0 && hs[k] == pans[k].1 && left > 0 && iw >= 30 {
                    hs[k] += 1;
                    left -= 1;
                }
            }
            // storage details, then programs (a third of the rest if there are
            // charts to grow, else all they can show)
            if hs[2] > 0 {
                let g = pans[2].1.saturating_sub(hs[2]).min(left);
                hs[2] += g;
                left -= g;
            }
            if hs[3] > 0 {
                let room = pans[3].1.saturating_sub(hs[3]);
                let g = if charts > 0 { left / 3 } else { left }.min(room);
                hs[3] += g;
                left -= g;
            }
            let grow: Vec<usize> = [1, 0]
                .into_iter()
                .filter(|&k| hs[k] > pans[k].1 + 1)
                .collect();
            match grow.len() {
                2 => {
                    hs[1] += left - left / 2;
                    hs[0] += left / 2;
                }
                1 => hs[grow[0]] += left,
                _ => {
                    let k = if hs[3] > 0 { 3 } else { 0 };
                    hs[k] += left;
                }
            }
            let mut y = r.y;
            let mut out = [Rect::default(); 4];
            for k in 0..4 {
                if hs[k] > 0 {
                    out[k] = at(r.x, y, r.w, hs[k]);
                    y += hs[k];
                }
            }
            l.mem = out[0];
            l.disk = out[1];
            l.fs = out[2];
            l.procs = out[3];
            l.gw = iw;
        }
        l
    }

    // ── animation ──────────────────────────────────────────────────────────

    /// New targets from the latest sample (`smooth`: springs follow; else jump).
    pub fn mem_retarget(&mut self, smooth: bool) {
        let m = &self.m;
        let a = &mut self.ma;
        let mi = m.mi;
        let f = |v: u64| v as f32;
        a.used.set(f(mi.used()), smooth);
        a.cache.set(f(mi.cache()), smooth);
        a.avail.set(f(mi.avail), smooth);
        a.swap.set(f(mi.swap_used()), smooth);
        a.swio.set(m.swin + m.swout, smooth);
        a.psi.set(m.psi, smooth);
        // new elements start at their value
        let fresh = |v: &mut Val, x: f32, sm: bool| {
            v.set(x, false);
            v.set(x, sm);
        };
        a.dimm.resize(m.dimms.len(), Val::default());
        for (v, &s) in a.dimm.iter_mut().zip(&m.dimms) {
            let t = slow_temp(s).unwrap_or(0.0);
            v.set(t, smooth && v.to != 0.0);
        }
        let n0 = a.disk.len();
        a.disk.resize(m.disks.len(), Default::default());
        for (k, (v, d)) in a.disk.iter_mut().zip(&m.disks).enumerate() {
            let sm = smooth && k < n0;
            let t = d.temp.and_then(slow_temp).unwrap_or(0.0);
            for (j, x) in [d.rd, d.wr, d.busy, t].into_iter().enumerate() {
                if sm && !(j == D_TEMP && v[j].to == 0.0) {
                    v[j].set(x, true);
                } else {
                    fresh(&mut v[j], x, smooth);
                }
            }
        }
        let n0 = a.fs.len();
        a.fs.resize(m.fs.len(), Val::default());
        for (k, (v, s)) in a.fs.iter_mut().zip(&m.fs).enumerate() {
            if k < n0 {
                v.set(f(s.used), smooth);
            } else {
                fresh(v, f(s.used), smooth);
            }
        }
        // programs: rows keep their place unless another one is clearly (3 %, and
        // 2 MB) bigger, so near-ties don't trade places back and forth
        let mut want: Vec<(u64, f32, usize)> = if a.split {
            m.procs
                .iter()
                .enumerate()
                .map(|(i, p)| (p.pid as u64 | 1 << 40, f(p.rss), i))
                .collect()
        } else {
            m.top
                .iter()
                .map(|&i| (name_key(&m.groups[i].name), f(m.groups[i].mem), i))
                .collect()
        };
        let mut rows = std::mem::take(&mut a.rows);
        rows.retain(|r| r.src != usize::MAX);
        crate::sys::isort(&mut rows, &|x, y| x.pos.to < y.pos.to);
        let mut out: Vec<PRow> = Vec::with_capacity(want.len() + 4);
        for r in rows.iter_mut() {
            if let Some(k) = want.iter().position(|w| w.0 == r.key) {
                let (_, v, src) = want.swap_remove(k);
                r.mem.set(v, smooth);
                r.src = src;
                r.fade.set(1.0, smooth);
                out.push(*r);
            }
        }
        let known = out.len();
        // newcomers in size order after the known ones (sorted in below)
        crate::sys::isort(&mut want, &|x, y| x.1 > y.1);
        for (key, v, src) in want {
            let mut r = PRow {
                key,
                src,
                ..Default::default()
            };
            fresh(&mut r.mem, v, smooth);
            r.fade.set(0.0, false);
            r.fade.set(1.0, smooth);
            out.push(r);
        }
        let ahead = |x: &PRow, y: &PRow| y.mem.to > x.mem.to * 1.03 + 2e6;
        let mut new = vec![false; out.len()];
        new[known..].iter_mut().for_each(|b| *b = true);
        for i in 1..out.len() {
            let mut j = i;
            while j > 0 && ahead(&out[j - 1], &out[j]) {
                out.swap(j - 1, j);
                new.swap(j - 1, j);
                j -= 1;
            }
        }
        for (i, r) in out.iter_mut().enumerate() {
            // a newcomer appears in place (fading in); the others slide
            r.pos.set(i as f32, smooth && !new[i]);
        }
        // rows that left fade out where they were (smooth), else go at once
        if smooth {
            for mut r in rows {
                if !out.iter().any(|o| o.key == r.key) && r.fade.x > 0.02 {
                    r.src = usize::MAX;
                    r.fade.set(0.0, true);
                    out.push(r);
                }
            }
        }
        a.rows = out;
    }

    /// Smooth mode: one frame of every memory-area spring and digit. Returns whether
    /// anything moves or changed.
    pub fn mem_step(&mut self, dt: f32) -> bool {
        let (xl, xt, xc) = (
            (-W_LOAD * dt).exp(),
            (-W_TEMP * dt).exp(),
            (-W_CLOCK * dt).exp(),
        );
        let total = self.m.mi.total as f32;
        let swt = self.m.mi.swap_total as f32;
        let a = &mut self.ma;
        let mut mv = false;
        let by =
            |v: &mut Val, full: f32| v.step(dt, W_LOAD, xl, 1.0) | v.digits(dt, bstep(v.x), full);
        // rates: a slower spring, and digits that hold small changes (≈ 1 s to read)
        let rt = |v: &mut Val| {
            v.step(dt, W_CLOCK, xc, 1.0) | v.digits_hold(dt, bstep(v.x).max(v.x.abs() * 0.04))
        };
        mv |= by(&mut a.used, total);
        mv |= by(&mut a.cache, total);
        mv |= by(&mut a.avail, total);
        mv |= by(&mut a.swap, swt);
        mv |= rt(&mut a.swio);
        for v in a.fs.iter_mut() {
            mv |= by(v, 0.0);
        }
        for r in a.rows.iter_mut() {
            mv |= by(&mut r.mem, 0.0);
            mv |= r.pos.step(dt, W_LOAD, xl, 0.002);
            mv |= r.fade.step(dt, W_LOAD, xl, 0.004);
        }
        let before = a.rows.len();
        a.rows.retain(|r| r.src != usize::MAX || r.fade.x > 0.02);
        mv |= a.rows.len() != before;
        for d in a.disk.iter_mut() {
            mv |= rt(&mut d[D_RD]);
            mv |= rt(&mut d[D_WR]);
            mv |= d[D_BUSY].step(dt, W_CLOCK, xc, 0.0005);
            mv |= d[D_BUSY].digits(dt, 0.01, 1.0);
            mv |= d[D_TEMP].step(dt, W_TEMP, xt, 0.01);
            mv |= d[D_TEMP].digits(dt, 1.0, 0.0);
        }
        mv |= a.psi.step(dt, W_LOAD, xl, 0.001);
        mv |= a.psi.digits(dt, 0.1, 0.0);
        for v in a.dimm.iter_mut() {
            mv |= v.step(dt, W_TEMP, xt, 0.01);
            mv |= v.digits(dt, 1.0, 0.0);
        }
        mv
    }

    fn mmemo(&mut self, slot: usize, key: u64) -> bool {
        if self.mcache.len() <= slot {
            self.mcache.resize(slot + 1, u64::MAX);
        }
        let ch = self.mcache[slot] != key;
        self.mcache[slot] = key;
        ch
    }

    /// History series for `n` braille half-columns (one point a second each): the
    /// points as they are, or in smooth mode with the live value at the head.
    pub(super) fn series(&self, out: &mut Vec<f32>, n: usize, h: &crate::sys::Ring<f32>, head: f32) {
        if !self.smooth() {
            h.tail(n, 0.0, out);
            return;
        }
        crate::anim::Curve {
            head,
            len: h.len(),
            pt: &|k| h.back(k),
            pad: 0.0,
        }
        .sample(out, n, 0.0, 1.0);
    }

    // ── drawing helpers ────────────────────────────────────────────────────

    /// One statistics row: label → value → [bar → %] → note, in aligned columns.
    /// `bar`: (fraction, width, ramp); width 0: none.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn srow(
        &self,
        scr: &mut Screen,
        x: usize,
        y: usize,
        maxx: usize,
        lab: (&str, u32),
        val: (&str, u32),
        bar: (f32, usize, &crate::color::Ramp),
        pct: (&str, u32),
        note: &[(&str, u32)],
    ) {
        let bg = surf(S_PANEL);
        let l = f!("{:<w$}", lab.0, w = LW);
        let v = rpad(&f!("{}", val.0), VW);
        let mut xx = Self::spans(scr, x, y, maxx, bg, &[(l.s(), lab.1), (v.s(), val.1)]);
        if bar.1 > 0 && xx + 1 + bar.1 <= maxx {
            xx = Self::spans(scr, xx, y, maxx, bg, &[(" ", 0)]);
            self.meter(scr, xx, y, bar.0, bar.1, bar.2, bg, LOAD);
            xx += bar.1;
            if !pct.0.is_empty() {
                let p = rpad(&f!("{}", pct.0), 5);
                xx = Self::spans(scr, xx, y, maxx, bg, &[(p.s(), pct.1)]);
            }
        }
        Self::line(scr, xx, y, maxx, bg, note);
    }

    /// Bar width for a row with a bar, a 5-wide % and `tail` more cells after it.
    pub(super) fn sbar_w(iw: usize, tail: usize) -> usize {
        iw.saturating_sub(LW + VW + 1 + 5 + tail).min(40)
    }

    /// Time labels in a panel's bottom border under a history chart: "2 min ago ·
    /// 1 min ago · now" where those ages fall (`spc` seconds per cell, the newest cell
    /// at gx + gw - 1), after an optional caption on the left.
    pub fn time_axis(&self, scr: &mut Screen, r: Rect, gx: usize, gw: usize, spc: f32, cap: &str) {
        if r.h < 2 || gw < 12 || r.w < 20 {
            return;
        }
        let bg = surf(S_PANEL);
        let (b, dim, mid) = (self.t.border, self.t.dim, self.t.mid);
        let y = r.y + r.h - 1;
        let x1 = r.x + r.w - 1;
        for x in r.x + 1..x1 {
            scr.put(x, y, '─', b, bg);
        }
        // "now" ends under the newest cell
        let end = (gx + gw).min(x1 - 1);
        let nx = end - 3;
        scr.put(nx - 1, y, ' ', b, bg);
        scr.text(nx, y, "now", mid, bg, x1);
        if end < x1 - 1 {
            scr.put(end, y, ' ', b, bg);
        }
        // ticks right to left, then the caption if it fits before the oldest one
        let span = gw as f32 * spc;
        let step = [15, 30, 60, 120, 300, 600, 1800]
            .into_iter()
            .find(|&s| s as f32 / spc >= 12.0 && s as f32 >= span / 4.0)
            .unwrap_or(3600);
        let lo = r.x + 2;
        let mut hi = nx - 2;
        let mut k = 1;
        while (k * step) as f32 <= span {
            let age = k * step;
            k += 1;
            let lab = ago(age);
            let n = lab.s().chars().count();
            let cx = gx + gw - 1 - ((age as f32 / spc).round() as usize).min(gw - 1);
            let lx = cx.saturating_sub(n / 2);
            if lx < lo + 1 || lx + n + 1 > hi {
                continue;
            }
            scr.put(lx - 1, y, ' ', b, bg);
            scr.text(lx, y, lab.s(), dim, bg, x1);
            scr.put(lx + n, y, ' ', b, bg);
            hi = lx - 2;
        }
        let cn = cap.chars().count();
        if cn > 0 && lo + cn + 1 < hi {
            scr.put(lo - 1, y, ' ', b, bg);
            let e = scr.text(lo, y, cap, dim, bg, x1);
            scr.put(e, y, ' ', b, bg);
        }
    }

    // ── drawing ────────────────────────────────────────────────────────────

    /// The memory area. `graphs`: also history bodies (not on bars-only frames).
    pub fn draw_mem(&mut self, scr: &mut Screen, full: bool, graphs: bool) {
        if full {
            self.mcache.iter_mut().for_each(|c| *c = u64::MAX);
        }
        let (a, m) = (&self.ma, &self.m);
        if a.disk.len() != m.disks.len()
            || a.fs.len() != m.fs.len()
            || a.dimm.len() != m.dimms.len()
        {
            self.mem_retarget(self.smooth());
        }
        let l = self.ml;
        if l.mem.w > 0 {
            self.draw_ram(scr, l.mem, full, graphs);
        }
        if l.disk.w > 0 {
            self.draw_disks(scr, l.disk, full, graphs);
        }
        if l.fs.w > 0 {
            self.draw_fs(scr, l.fs, full);
        }
        if l.procs.w > 0 {
            self.draw_procs(scr, l.procs, full);
        }
    }

    /// RAM health: 0 plenty free, 1 getting full / swapping / waiting, 2 nearly full.
    pub(super) fn ram_level(&self) -> u8 {
        let a = &self.ma;
        let total = (self.m.mi.total as f32).max(1.0);
        // (swap traffic alone, with plenty available, is one memory-limited group
        // of programs, not the machine running short: it shows on the swap row)
        let short = a.avail.x / total < 0.2;
        level(a.used.x / total, RAM_WARN, RAM_CRIT)
            .max(if short {
                level(a.swio.x, SWAP_WARN, SWAP_CRIT)
            } else {
                0
            })
            .max(level(a.psi.x, PSI_WARN, PSI_CRIT))
    }

    /// RAM chart geometry: (y-axis gutter, right label gutter).
    fn ram_gut(iw: usize) -> (usize, usize) {
        (5, if iw >= 40 { 5 } else { 0 })
    }

    fn draw_ram(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let total = (self.m.mi.total as f32).max(1.0);
        let fu = self.ma.used.x / total;
        let nst = self.ram_stats();
        // chart: a gap, a caption and ≥ 5 rows under the statistics
        let chart = ih >= nst + 7 && iw >= 30;
        let gh = if chart { ih - nst - 2 } else { 0 };
        let lv = self.ram_level();
        let tk = mixk(&[self.ma.used.n.to_bits() as u64, iw as u64, lv as u64]);
        if self.mmemo(K_MTITLE, tk) || full {
            let th = &self.t;
            let a = &self.ma;
            let up = a.used.n / total;
            let p = num(" ", up * 100.0, 0, "% in use");
            let ps = num(" ", up * 100.0, 0, "%");
            let ut = f!(" · {}", of_size(a.used.n, total).s());
            let ul = level(up, RAM_WARN, RAM_CRIT);
            let verdict = match (lv, ul) {
                (0, _) => " · plenty free",
                (1, 1) => " · getting full",
                (1, _) => " · RAM is tight",
                (_, 2) => " · nearly full",
                _ => " · short of RAM",
            };
            let c = self.hcol(ul) | BOLD;
            let vc = self.hcol(lv);
            let vs: [&[(&str, u32)]; 4] = [
                &[
                    ("RAM", th.title | BOLD),
                    (p.s(), c),
                    (ut.s(), th.text),
                    (verdict, vc),
                ],
                &[("RAM", th.title | BOLD), (p.s(), c), (ut.s(), th.text)],
                &[("RAM", th.title | BOLD), (p.s(), c)],
                &[("RAM", th.title | BOLD), (ps.s(), c)],
            ];
            let title = Self::pick(r.w, &vs);
            if full {
                self.panel(scr, r, title, "");
                if chart {
                    let (gut, rg) = Self::ram_gut(iw);
                    self.time_axis(scr, r, ir.x + gut, iw - gut - rg, 2.0, "");
                }
            } else {
                self.panel_top(scr, r, title);
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let a = &self.ma;
        let mut k = mixk(&[fill16(fu, iw) as u64, iw as u64, ih as u64]);
        for v in [&a.used, &a.cache, &a.avail, &a.swap, &a.swio, &a.psi] {
            k = mixk(&[k, v.n.to_bits() as u64]);
        }
        let swt = self.m.mi.swap_total as f32;
        k = mixk(&[k, fill16(a.swap.x / swt.max(1.0), iw) as u64]);
        for d in &a.dimm {
            k = mixk(&[k, d.n.to_bits() as u64]);
        }
        if !chart {
            // the trend line moves with the history
            k = mixk(&[k, self.m.hist_n, self.body_key(1)]);
        }
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        if self.mmemo(K_MEM, k) || full {
            self.ram_rows(scr, ir, chart);
        }
        if chart && (graphs || full) {
            let gy = ir.y + nst + 1;
            let body = self.body_key(2);
            if self.mmemo(K_MGRAPH, mixk(&[body, gh as u64, iw as u64])) || full {
                let th = &self.t;
                let (then, w) = self.ram_trend(60);
                let (p0, p1) = (
                    (then * 100.0).round() as u32,
                    (self.ma.used.n / total * 100.0).round() as u32,
                );
                let since = if self.m.h_used.len() >= 60 {
                    "1 min ago"
                } else {
                    "at start"
                };
                let cap = f!(
                    "RAM in use over time · {}: {}% {}, {}% now",
                    w,
                    p0,
                    since,
                    p1
                );
                let cap = if cap.n <= iw {
                    cap
                } else {
                    f!("RAM over time · {} {}% → {}%", w, p0, p1)
                };
                Self::line(scr, ir.x, gy, maxx, bg, &[(cap.s(), th.mid)]);
                let (gut, rg) = Self::ram_gut(iw);
                let gw = iw - rg;
                let mut vals = std::mem::take(&mut self.vals);
                let dot = 1.0 / (4 * (gh - 1)) as f32;
                let now = self.ma.used.n / total;
                if (now - self.ma.head).abs() > 0.6 * dot || !self.smooth() {
                    self.ma.head = now;
                }
                self.series(&mut vals, 2 * (gw - gut), &self.m.h_used, self.ma.head);
                self.axis_graph(
                    scr,
                    ir.x,
                    gy + 1,
                    gw,
                    gh - 1,
                    &vals,
                    1.0,
                    &self.t.load,
                    if gh > 9 {
                        &[
                            (1.0, "100%"),
                            (0.75, "75%"),
                            (0.5, "50%"),
                            (0.25, "25%"),
                            (0.0, "0%"),
                        ]
                    } else {
                        &[(1.0, "100%"), (0.5, "50%"), (0.0, "0%")]
                    },
                );
                self.vals = vals;
                if rg > 0 {
                    // the current value at the right edge, at its height
                    let h = gh - 1;
                    let row = ((1.0 - fu) * h as f32 - 0.5)
                        .round()
                        .max(0.0)
                        .min(h as f32 - 1.0) as usize;
                    // (the same number as the headline)
                    let p = num("", self.ma.used.n / total * 100.0, 0, "%");
                    let c = self.hcol(level(fu, RAM_WARN, RAM_CRIT)) | BOLD;
                    for yy in 0..h {
                        let y = gy + 1 + yy;
                        if yy == row {
                            Self::line(scr, ir.x + gw, y, maxx, bg, &[(" ", 0), (p.s(), c)]);
                        } else {
                            Self::line(scr, ir.x + gw, y, maxx, bg, &[]);
                        }
                    }
                }
            }
        }
    }

    /// The RAM panel's statistics (and, without a chart, a trend line if there's room).
    fn ram_rows(&self, scr: &mut Screen, ir: Rect, chart: bool) {
        let th = &self.t;
        let a = &self.ma;
        let m = &self.m;
        let (iw, ih) = (ir.w, ir.h);
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        let total = (m.mi.total as f32).max(1.0);
        let swt = m.mi.swap_total as f32;
        let fu = a.used.x / total;
        let wide = iw >= 56;
        // rows by priority, shown in a fixed order
        const INUSE: usize = 0;
        const AVAIL: usize = 1;
        const CACHE: usize = 2;
        const SWAP: usize = 3;
        const WAIT: usize = 4;
        const TEMP: usize = 5;
        const TREND: usize = 6;
        let mut on = [false; 7];
        let mut left = ih;
        for p in [INUSE, SWAP, AVAIL, TREND, CACHE, TEMP, WAIT] {
            let ok = match p {
                WAIT => m.has_psi,
                TEMP => !m.dimms.is_empty(),
                TREND => !chart && iw >= 30,
                _ => true,
            };
            if ok && left > 0 {
                on[p] = true;
                left -= 1;
            }
        }
        let mut y = ir.y;
        // in use
        let ul = level(fu, RAM_WARN, RAM_CRIT);
        let uv = bsize(a.used.n, false);
        let up = num("", a.used.n / total * 100.0, 0, "%");
        let of = f!("  of {}", bsize(total, false).s());
        let tail = if wide { of.n } else { 0 };
        let bw = Self::sbar_w(iw, tail).max(4);
        self.srow(
            scr,
            ir.x,
            y,
            maxx,
            ("in use", th.text),
            (uv.s(), th.fg | BOLD),
            (fu, bw, &th.load),
            (up.s(), self.hcol(ul) | BOLD),
            &[(if wide { of.s() } else { "" }, th.dim)],
        );
        y += 1;
        let none = (0.0, 0, &th.load);
        if on[AVAIL] {
            let v = bsize(a.avail.n, false);
            let p = num("  ", a.avail.n / total * 100.0, 0, "%");
            let hint = if iw >= 48 {
                " · free for programs now"
            } else {
                ""
            };
            self.srow(
                scr,
                ir.x,
                y,
                maxx,
                ("available", th.text),
                (v.s(), th.fg),
                none,
                ("", 0),
                &[(p.s(), th.text), (hint, th.dim)],
            );
            y += 1;
        }
        if on[CACHE] {
            let v = bsize(a.cache.n, false);
            let hint = if iw >= 60 {
                "  files kept for speed, part of available"
            } else if iw >= 40 {
                "  part of available"
            } else {
                ""
            };
            self.srow(
                scr,
                ir.x,
                y,
                maxx,
                ("cache", th.text),
                (v.s(), th.text),
                none,
                ("", 0),
                &[(hint, th.dim)],
            );
            y += 1;
        }
        if on[SWAP] {
            if swt > 0.0 {
                let sl = level(a.swio.x, SWAP_WARN, SWAP_CRIT);
                let st = if a.swio.n < IDLE_BPS {
                    f!("  no activity")
                } else if sl == 0 {
                    f!("  little activity")
                } else {
                    f!("  busy {}", rate(a.swio.n, false).s())
                };
                let why = if iw < 50 {
                    ""
                } else if sl == 0 {
                    " (fine)"
                } else if a.avail.x / total < 0.2 {
                    " (RAM short)"
                } else {
                    ""
                };
                let lab = if m.zram { "swap zram" } else { "swap" };
                let bw = Self::sbar_w(iw, st.n + why.len()).max(4);
                let v = bsize(a.swap.n, false);
                let p = num("", a.swap.n / swt * 100.0, 0, "%");
                self.srow(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    (lab, th.text),
                    (v.s(), th.text),
                    (a.swap.x / swt, bw, &th.load),
                    (p.s(), th.text),
                    &[(st.s(), self.hcol(sl)), (why, th.dim)],
                );
            } else {
                self.srow(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    ("swap", th.text),
                    ("none", th.mid),
                    none,
                    ("", 0),
                    &[("  no swap set up", th.dim)],
                );
            }
            y += 1;
        }
        if on[WAIT] {
            let pl = level(a.psi.n, PSI_WARN, PSI_CRIT);
            let v = pct1(a.psi.n / 100.0);
            let hint = if iw >= 50 {
                "  of time programs waited for RAM"
            } else {
                "  waiting for RAM"
            };
            self.srow(
                scr,
                ir.x,
                y,
                maxx,
                ("waiting", th.text),
                (v.s(), self.hcol(pl)),
                none,
                ("", 0),
                &[(hint, th.dim)],
            );
            y += 1;
        }
        if on[TEMP] {
            // the warmest module, then each one: "41°C · OK  2 modules: 40, 41°C"
            let mut each = F::new();
            let mut mx = 0.0f32;
            let nm = a.dimm.len();
            if nm > 1 {
                each.str(f!("  {} RAM sticks: ", nm).s());
            } else {
                each.str("  RAM stick");
            }
            for (i, d) in a.dimm.iter().enumerate() {
                mx = mx.max(d.n);
                if nm > 1 {
                    if i > 0 {
                        each.str(", ");
                    }
                    if d.n > 0.0 {
                        each.str(num("", d.n, 0, "").s());
                    } else {
                        each.str("–");
                    }
                }
            }
            if nm > 1 {
                each.str("°C");
            }
            let tl = level(mx, DIMM_WARN, DIMM_CRIT);
            let v = if mx > 0.0 {
                num("", mx, 0, "°C")
            } else {
                f!("–")
            };
            let ok = [" · OK", " · warm", " · hot"][tl as usize];
            let hint = if iw >= 46 { each.s() } else { "" };
            self.srow(
                scr,
                ir.x,
                y,
                maxx,
                ("RAM temp", th.text),
                (v.s(), self.hcol(tl)),
                none,
                ("", 0),
                &[(ok, self.hcol(tl)), (hint, th.dim)],
            );
            y += 1;
        }
        if on[TREND] {
            // a minute first, with the trend in words: long, short, or none
            let p1 = (a.used.n / total * 100.0).round() as u32;
            let (span, words) = [(20, 2), (11, 1), (0, 0)]
                .into_iter()
                .map(|(tl, k)| (Self::strip_span(iw, tl), k))
                .find(|&(s, _)| s >= 60)
                .unwrap_or((Self::strip_span(iw, 11), 1));
            let (then, w) = self.ram_trend(span);
            let p0 = (then * 100.0).round() as u32;
            let short = match w {
                "rising" => "up",
                "falling" => "down",
                _ => "flat",
            };
            let t = match words {
                2 => f!(" · {} {}% → {}%", w, p0, p1),
                1 => f!(" {} {}→{}%", short, p0, p1),
                _ => F::new(),
            };
            let h = &m.h_used;
            let c = |v: f32| th.load.at(v);
            let fu = a.used.n / total;
            self.strip_row(
                scr,
                ir.x,
                y,
                iw,
                h.len(),
                (fu, c(fu)),
                &|k| (h.back(k), c(h.back(k))),
                &[(t.s(), th.text)],
            );
            y += 1;
        }
        let stop = if chart {
            ir.y + self.ram_stats() + 1
        } else {
            ir.y + ih
        };
        while y < stop.min(ir.y + ih) {
            Self::line(scr, ir.x, y, maxx, bg, &[]);
            y += 1;
        }
    }

    /// RAM in use (fraction) `secs` ago (the oldest point if the history is shorter),
    /// and how it changed since in words: "rising", "falling" or "steady" (± 2 points).
    pub(super) fn ram_trend(&self, secs: usize) -> (f32, &'static str) {
        let h = &self.m.h_used;
        let total = (self.m.mi.total as f32).max(1.0);
        let now = self.ma.used.n / total;
        let then = if h.len() == 0 {
            now
        } else {
            h.back(secs.min(h.len()) - 1)
        };
        let d = now - then;
        let w = if d >= 0.02 {
            "rising"
        } else if d <= -0.02 {
            "falling"
        } else {
            "steady"
        };
        (then, w)
    }

    /// The span of a one-line history `iw` wide with `tl` cells of words after it: a
    /// round 2 min, 1 min or 30 s of 2 s cells (0: none fits).
    fn strip_span(iw: usize, tl: usize) -> usize {
        let room = iw.saturating_sub(LW + 1 + 4 + tl);
        [120usize, 60, 30]
            .into_iter()
            .find(|&s| s / 2 <= room)
            .unwrap_or(0)
    }

    /// A one-line history where a chart doesn't fit: "1 min ago ▁▁▂▃▃▅ now" and then
    /// in words what it says (`tail`). Cells are 2 s means aligned to the
    /// history count (they step once per 2 s); `cell(k)` gives point k's (value 0..1,
    /// colour), k = 0 the newest point; the newest cell is `head`.
    #[allow(clippy::too_many_arguments)]
    fn strip_row(
        &self,
        scr: &mut Screen,
        x: usize,
        y: usize,
        iw: usize,
        len: usize,
        head: (f32, u32),
        pt: &dyn Fn(usize) -> (f32, u32),
        tail: &[(&str, u32)],
    ) {
        let th = &self.t;
        let bg = surf(S_PANEL);
        let secs = Self::strip_span(iw, Self::spans_len(tail));
        if secs == 0 {
            return;
        }
        let lag = (self.m.hist_n % 2) as usize;
        let w = secs / 2;
        let n = w.min(1 + len.saturating_sub(lag) / 2);
        let l = f!("{:<w$}", ago(secs).s(), w = LW + 1);
        let mut xx = scr.text(x, y, l.s(), th.dim, bg, x + iw);
        while xx < x + LW + 1 + w - n {
            scr.put(xx, y, ' ', NOPATCH, bg);
            xx += 1;
        }
        for i in (0..n).rev() {
            let (v, c) = if i == 0 {
                head
            } else {
                let p = lag + 2 * (i - 1);
                let (a, c) = pt(p);
                ((a + pt(p + 1).0) / 2.0, c)
            };
            let lv = (v.clamp(0.0, 1.0) * 8.0).round() as usize;
            if lv == 0 {
                scr.put(xx, y, '▁', TRACK, bg);
            } else {
                scr.put(xx, y, BLOCKS[lv.min(8) - 1], c, bg);
            }
            xx += 1;
        }
        let xx = scr.text(xx, y, " now", th.mid, bg, x + iw);
        Self::line(scr, xx, y, x + iw, bg, tail);
    }

    /// Reading and writing colours: the clock ramp's blue and the load ramp's orange,
    /// or (picture palettes can make those alike) the most distinct pair of the
    /// theme's other colours, so the two always tell apart.
    pub(super) fn rw_cols(&self) -> (u32, u32) {
        let t = &self.t;
        let rs = [t.freq.at(0.35), t.temp.at(0.0), t.accent, t.good];
        let ws = [t.load.at(0.68), t.warn, t.accent2, t.crit];
        let d = |a: u32, b: u32| {
            let (a, b) = (crate::color::ch(a), crate::color::ch(b));
            (0..3)
                .map(|i| (a[i] as i32 - b[i] as i32).pow(2))
                .sum::<i32>()
        };
        let mut best = (0, rs[0], ws[0]);
        for &r in &rs {
            for &w in &ws {
                let k = d(r, w);
                if k >= 110 * 110 {
                    return (r, w);
                }
                if k > best.0 {
                    best = (k, r, w);
                }
            }
        }
        (best.1, best.2)
    }

    fn draw_disks(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let nd = self.m.disks.len();
        let one = nd == 1;
        let ds = self.disk_stats();
        // rows: reading, writing, busy, then a one-line history, then temperature
        // (else it joins the busy row); several disks: all of them or one line each
        let stats = if one { ih >= 3 } else { nd == 2 && ih >= ds };
        // rows above the chart / trend line: the statistics, or one line per disk
        let top = if stats { ds } else { nd.min(ih) };
        // chart (all disks together): a gap, caption, legend, ≥ 4 rows, summary
        let chart = ih >= top + 8 && iw >= 36;
        let trend = !chart && iw >= 30 && ih >= if one && stats { 4 } else { top + 1 };
        let trow = !one || chart || ih >= 4 + trend as usize;
        // headline: what the disk(s) are doing now
        let (mut rd, mut wr, mut busy) = (0.0f32, 0.0f32, 0.0f32);
        for v in &self.ma.disk {
            rd += v[D_RD].n;
            wr += v[D_WR].n;
            busy = busy.max(v[D_BUSY].n);
        }
        let idle = rd < IDLE_BPS && wr < IDLE_BPS;
        let tk = mixk(&[
            rd.to_bits() as u64,
            wr.to_bits() as u64,
            (busy * 100.0).round() as u64,
            iw as u64,
        ]);
        let (rc, wc) = self.rw_cols();
        if self.mmemo(K_DTITLE, tk) || full {
            let th = &self.t;
            let (rr, wrr) = (rate(rd, false), rate(wr, false));
            let (r1, w1) = (f!(" reading {}", rr.s()), f!(" · writing {}", wrr.s()));
            let bz = num(" · ", busy * 100.0, 0, "% busy");
            let lab = if one { "disk" } else { "disks" };
            let bc = th.load.at(busy);
            let vs: [&[(&str, u32)]; 4] = [
                &[
                    (lab, th.title | BOLD),
                    (r1.s(), rc | BOLD),
                    (w1.s(), wc | BOLD),
                    (bz.s(), bc),
                ],
                &[
                    (lab, th.title | BOLD),
                    (r1.s(), rc | BOLD),
                    (w1.s(), wc | BOLD),
                ],
                &[(lab, th.title | BOLD), (r1.s(), rc | BOLD)],
                &[(lab, th.title | BOLD)],
            ];
            let vi: [&[(&str, u32)]; 2] = [
                &[
                    (lab, th.title | BOLD),
                    (" idle", th.good),
                    (" · nothing being read or written", th.dim),
                ],
                &[(lab, th.title | BOLD), (" idle", th.good)],
            ];
            let title = if idle {
                Self::pick(r.w, &vi)
            } else {
                Self::pick(r.w, &vs)
            };
            if full {
                self.panel(scr, r, title, "");
                if chart {
                    let (gx, gw, spc) = self.dchart_geo(ir);
                    self.time_axis(scr, r, gx, gw, spc, "");
                }
            } else {
                self.panel_top(scr, r, title);
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let a = &self.ma;
        let mut k = mixk(&[iw as u64, ih as u64]);
        for d in &a.disk {
            for v in d {
                k = mixk(&[k, v.n.to_bits() as u64]);
            }
            k = mixk(&[k, fill16(d[D_BUSY].x, iw) as u64]);
        }
        if trend {
            k = mixk(&[k, self.m.hist_n, self.body_key(1)]);
        }
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        if self.mmemo(K_DISK, k) || full {
            let th = &self.t;
            let mut y = ir.y;
            for (i, d) in self.m.disks.iter().enumerate() {
                if y >= ir.y + ih {
                    break;
                }
                let v = &self.ma.disk[i];
                let (rr, wrr) = (rate(v[D_RD].n, false), rate(v[D_WR].n, false));
                let bp = num("", v[D_BUSY].n * 100.0, 0, "%");
                if !stats {
                    // one line per disk
                    let nm = if one { F::new() } else { f!("{} ", d.name) };
                    let short = iw < 52;
                    Self::line(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        bg,
                        &[
                            (nm.s(), th.fg),
                            (if short { "read " } else { "reading " }, th.dim),
                            (rr.s(), rc),
                            (if short { " · write " } else { " · writing " }, th.dim),
                            (wrr.s(), wc),
                            (" · busy ", th.dim),
                            (bp.s(), th.load.at(v[D_BUSY].x)),
                        ],
                    );
                    y += 1;
                    continue;
                }
                if !one {
                    let md = f!("  {}", d.model);
                    Self::line(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        bg,
                        &[(&d.name, th.fg | BOLD), (md.s(), th.mid)],
                    );
                    y += 1;
                }
                let (hr, hw) = if iw >= 44 {
                    ("  programs loading files", "  programs saving files")
                } else {
                    ("", "")
                };
                let none = (0.0, 0, &th.load);
                self.srow(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    ("reading", rc),
                    (rr.s(), th.fg | BOLD),
                    none,
                    ("", 0),
                    &[(hr, th.dim)],
                );
                self.srow(
                    scr,
                    ir.x,
                    y + 1,
                    maxx,
                    ("writing", wc),
                    (wrr.s(), th.fg | BOLD),
                    none,
                    ("", 0),
                    &[(hw, th.dim)],
                );
                let t = v[D_TEMP].n;
                let tl = level(t, DISK_WARN, DISK_CRIT);
                let tv = num("", t, 0, "°C");
                let ok = [" · OK", " · warm", " · too hot"][tl as usize];
                // (no temperature row: it follows the busy bar)
                let inl = !trow && t > 0.0;
                let what = !inl && iw >= 56;
                let tail = if inl {
                    3 + tv.n + ok.len()
                } else if what {
                    16
                } else {
                    0
                };
                let bw = Self::sbar_w(iw, tail).max(4) + 5;
                self.srow(
                    scr,
                    ir.x,
                    y + 2,
                    maxx,
                    ("busy", th.text),
                    (bp.s(), th.load.at(v[D_BUSY].x) | BOLD),
                    (v[D_BUSY].x, bw, &th.load),
                    ("", 0),
                    &[
                        (if inl { "   " } else { "" }, 0),
                        (if inl { tv.s() } else { "" }, self.hcol(tl) | BOLD),
                        (if inl { ok } else { "" }, self.hcol(tl)),
                        (if what { "  of the time" } else { "" }, th.dim),
                    ],
                );
                let size = bsize(d.size as f32, false);
                if !trow {
                    y += 3;
                    continue;
                }
                if t > 0.0 {
                    let md = f!("  {} {} · {}", d.name, size.s(), d.model);
                    self.srow(
                        scr,
                        ir.x,
                        y + 3,
                        maxx,
                        ("temp", th.text),
                        (tv.s(), self.hcol(tl) | BOLD),
                        none,
                        ("", 0),
                        &[(ok, self.hcol(tl)), (md.s(), th.dim)],
                    );
                } else {
                    let md = f!("  {} · {}", size.s(), d.model);
                    self.srow(
                        scr,
                        ir.x,
                        y + 3,
                        maxx,
                        ("drive", th.text),
                        (&d.name, th.text),
                        none,
                        ("", 0),
                        &[(md.s(), th.dim)],
                    );
                }
                y += 4;
            }
            if trend && y < ir.y + ih {
                // the span that fits with the longest words, then the words for it
                // a minute or more first, with as many words as fit
                let (span, words) = [(27, 2), (16, 1), (0, 0)]
                    .into_iter()
                    .map(|(tl, k)| (Self::strip_span(iw, tl), k))
                    .find(|&(s, _)| s >= 60)
                    .unwrap_or((Self::strip_span(iw, 16), 1));
                let (pk, age, _, _) = self.disk_peak(span);
                let sc = nice_rate(pk);
                let t = if words == 0 {
                    F::new()
                } else if pk < IDLE_BPS {
                    f!(" · idle")
                } else if age == 0 {
                    f!(" · peak {} now", rate(pk, false).s())
                } else if words == 2 {
                    f!(" · peak {} {}", rate(pk, false).s(), ago(age).s())
                } else {
                    f!(" · peak {}", rate(pk, false).s())
                };
                let (rc, wc) = self.rw_cols();
                let col = |r: f32, w: f32| {
                    let v = if r + w < IDLE_BPS {
                        0.0
                    } else {
                        ((r + w) / sc).max(0.07)
                    };
                    (v, if r >= w { rc } else { wc })
                };
                let (r0, w0) = self.disk_now();
                self.strip_row(
                    scr,
                    ir.x,
                    y,
                    iw,
                    self.disk_len(),
                    col(r0, w0),
                    &|k| {
                        let (r, w) = self.disk_pt(k);
                        col(r, w)
                    },
                    &[(t.s(), th.text)],
                );
                y += 1;
            }
            let stop = if chart { ir.y + top + 1 } else { ir.y + ih };
            while y < stop {
                Self::line(scr, ir.x, y, maxx, bg, &[]);
                y += 1;
            }
        }
        if chart && (graphs || full) {
            let sk = mixk(&[self.body_key(0), self.m.hist_n, iw as u64, ih as u64]);
            if self.mmemo(K_DGRAPH, sk) || full {
                self.disk_chart(scr, ir, ir.y + top + 1, ir.y + ih, full);
            }
        }
    }

    /// Disk chart geometry in the panel's inner rect: (x of the first column, columns,
    /// seconds per column).
    fn dchart_geo(&self, ir: Rect) -> (usize, usize, f32) {
        let gut = 10;
        let rg = if ir.w >= 48 { 11 } else { 0 };
        let gw = ir.w.saturating_sub(gut + rg).max(1);
        // 2-5 s a column, enough for ~2 minutes
        let b = 120usize.div_ceil(gw).clamp(2, 5);
        (ir.x + gut, gw, b as f32)
    }

    /// Column `c` of the disk activity (all disks): 0 is now (the rates as shown),
    /// then mean read / write rates over whole `b`-second buckets of the history,
    /// aligned to the history count, so the columns step left once per bucket and
    /// never change in between.
    pub(super) fn disk_col(&self, c: usize, b: usize) -> Option<Col> {
        if c == 0 {
            let (rd, wr) = self.disk_now();
            return Some(Col { rd, wr });
        }
        let lag = (self.m.hist_n % b as u64) as usize;
        let (lo, hi) = (lag + (c - 1) * b, lag + c * b);
        if hi > self.disk_len() {
            return None;
        }
        let (mut r, mut w) = (0.0, 0.0);
        for p in lo..hi {
            let (pr, pw) = self.disk_pt(p);
            r += pr;
            w += pw;
        }
        Some(Col {
            rd: r / b as f32,
            wr: w / b as f32,
        })
    }

    /// History point `p` (0 = newest) of all disks together: read, write bytes/s.
    pub(super) fn disk_pt(&self, p: usize) -> (f32, f32) {
        self.m.disks.iter().fold((0.0, 0.0), |a, d| {
            (a.0 + d.h_rd.back(p), a.1 + d.h_wr.back(p))
        })
    }

    /// The live rates of all disks together, as the numbers show them (held, so the
    /// newest column doesn't flicker).
    pub(super) fn disk_now(&self) -> (f32, f32) {
        self.ma
            .disk
            .iter()
            .fold((0.0, 0.0), |a, v| (a.0 + v[D_RD].n, a.1 + v[D_WR].n))
    }

    pub(super) fn disk_len(&self) -> usize {
        self.m.disks.iter().map(|d| d.h_rd.len()).min().unwrap_or(0)
    }

    /// Over the last `secs` (all disks): the highest one-second total rate and its
    /// age (s), and the bytes read and written.
    fn disk_peak(&self, secs: usize) -> (f32, usize, f32, f32) {
        let (r0, w0) = self.disk_now();
        let (mut pk, mut age) = (r0 + w0, 0);
        let (mut tr, mut tw) = (0.0, 0.0);
        for p in 0..secs.min(self.disk_len()) {
            let (r, w) = self.disk_pt(p);
            tr += r;
            tw += w;
            if r + w > pk {
                pk = r + w;
                age = p + 1;
            }
        }
        (pk, age, tr, tw)
    }

    /// The disk activity chart in rows y0..y1 of the panel: caption, legend, stacked
    /// columns (reading below, writing on top) on a round MB/s scale with the current
    /// rates at the right edge, then the totals and the peak in words.
    fn disk_chart(&self, scr: &mut Screen, ir: Rect, y0: usize, y1: usize, full: bool) {
        let (gx, gw, spc) = self.dchart_geo(ir);
        let b = spc as usize;
        let th = &self.t;
        let bg = surf(S_PANEL);
        let maxx = ir.x + ir.w;
        let (rc, wc) = self.rw_cols();
        let all = if self.m.disks.len() > 1 {
            "all disks: "
        } else {
            "disk "
        };
        let span = dur(gw * b);
        let cap = f!(
            "{}activity, last {} · each column = {} s average",
            all,
            span.s(),
            b
        );
        let cap = if cap.n <= ir.w {
            cap
        } else {
            f!("{}activity, last {} · {} s a column", all, span.s(), b)
        };
        Self::line(scr, ir.x, y0, maxx, bg, &[(cap.s(), th.mid)]);
        let l1: [(&str, u32); 4] = [
            ("■ ", rc),
            ("reading (loading files)  ", th.text),
            ("■ ", wc),
            ("writing (saving files), stacked", th.text),
        ];
        let l2: [(&str, u32); 4] = [
            ("■ ", rc),
            ("reading (loading files) ", th.text),
            ("■ ", wc),
            ("writing (saving files)", th.text),
        ];
        let l3: [(&str, u32); 4] = [
            ("■ ", rc),
            ("reading  ", th.text),
            ("■ ", wc),
            ("writing, stacked", th.text),
        ];
        let vs: [&[(&str, u32)]; 3] = [&l1, &l2, &l3];
        let legend = vs
            .iter()
            .find(|v| Self::spans_len(v) <= ir.w)
            .unwrap_or(&vs[2]);
        Self::line(scr, ir.x, y0 + 1, maxx, bg, legend);
        let mut ch = y1.saturating_sub(y0 + 3);
        if ch == 0 {
            return;
        }
        // totals and the peak on two lines when the chart has rows to spare
        let two = ch >= 9 && ir.w < 72;
        if two {
            ch -= 1;
        }
        let cy = y0 + 2;
        // columns and scale
        let mut cols = [Col::default(); 256];
        let n = gw.min(256);
        let mut have = 0;
        let mut top = 0.0f32;
        for (c, slot) in cols.iter_mut().enumerate().take(n) {
            match self.disk_col(c, b) {
                Some(v) => {
                    *slot = v;
                    have = c + 1;
                    top = top.max(v.rd + v.wr);
                }
                None => break,
            }
        }
        let sc = nice_rate(top);
        // the newest column moves only by more than half an eighth of a cell (its
        // two rates change at different moments: their sum mustn't flicker at an
        // edge)
        let (hr, hw, ht) = self.ma.dhead.get();
        let now = cols[0];
        let q = sc / (8 * y1.saturating_sub(y0 + 3).max(1)) as f32;
        let t = crate::term::mono();
        if !self.smooth()
            || (now.rd + now.wr - hr - hw).abs() > 0.6 * q && t - ht >= 150_000_000
        {
            self.ma.dhead.set((now.rd, now.wr, t));
        } else {
            cols[0] = Col { rd: hr, wr: hw };
        }
        // y-axis: top, half, 0
        let lt = rate_label(sc);
        let lh = rate_label(sc / 2.0);
        let half = if ch >= 5 { ch / 2 } else { usize::MAX };
        for r in 0..ch {
            let lab = if r == 0 {
                lt.s()
            } else if r == half {
                lh.s()
            } else if r == ch - 1 {
                "0"
            } else {
                ""
            };
            let l = rpad(&f!("{}", lab), gx - ir.x - 1);
            let y = cy + r;
            scr.text(ir.x, y, l.s(), th.dim, bg, gx);
            scr.put(
                gx - 1,
                y,
                if lab.is_empty() { '│' } else { '┤' },
                th.border,
                bg,
            );
        }
        // columns (newest at the right), stacked in eighths: reading below writing
        let e8 = (ch * 8) as f32;
        let mut h8 = [(0, 0, false); 256];
        for (c, v) in cols.iter().enumerate().take(n) {
            let tot = v.rd + v.wr;
            let t8 = if c < have && tot >= IDLE_BPS {
                ((tot / sc).min(1.0) * e8).round().max(1.0) as usize
            } else {
                0
            };
            let r8 = if tot > 0.0 {
                (t8 as f32 * v.rd / tot).round() as usize
            } else {
                0
            };
            h8[c] = (t8 as u16, r8 as u16, c < have);
        }
        self.col_chart(scr, gx, cy, ch, &h8[..n], (rc, wc), half, full);
        // "idle" over long quiet stretches
        let mut idle_run = 0usize;
        for (c, &(t8, _, has)) in h8.iter().enumerate().take(n) {
            if has && t8 == 0 {
                idle_run += 1;
            } else {
                idle_run = 0;
            }
            if idle_run == 10 && ch >= 3 {
                let x = gx + gw - 1 - c;
                scr.text(x + 3, cy + ch - 1, "idle", th.dim, bg, x + 7);
            }
        }
        // the current rates at the right edge
        if gx + gw < maxx {
            let (r, w) = self
                .ma
                .disk
                .iter()
                .fold((0.0, 0.0), |a, v| (a.0 + v[D_RD].n, a.1 + v[D_WR].n));
            let (r, w) = (rate(r, false), rate(w, false));
            let rx = gx + gw;
            for row in 0..ch {
                let y = cy + row;
                let sp: [(&str, u32); 2] = match row {
                    0 => [(" now", th.mid), ("", 0)],
                    1 => [(" ", 0), (r.s(), rc | BOLD)],
                    2 => [(" ", 0), (w.s(), wc | BOLD)],
                    _ => [("", 0), ("", 0)],
                };
                Self::line(scr, rx, y, maxx, bg, &sp);
            }
        }
        // totals and the peak, in words
        let span = gw * b;
        let (pk, age, tr, tw) = self.disk_peak(span);
        let secs = span.min(self.disk_len()).max(1);
        let s1 = f!(
            "read {} · written {} {} {}",
            bsize(tr, false).s(),
            bsize(tw, false).s(),
            "in the last",
            dur(secs).s()
        );
        let s2 = if pk < IDLE_BPS {
            F::new()
        } else if age == 0 {
            f!(" · peak {} now", rate(pk, false).s())
        } else {
            f!(" · peak {} {}", rate(pk, false).s(), ago(age).s())
        };
        let sy = y1 - 1;
        // a minute ago against now, in words (the mean of 58-62 s back)
        let n = self.disk_len();
        let then = if n >= 62 {
            let (r, w) = (58..62)
                .map(|p| self.disk_pt(p))
                .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
            Some((r + w) / 4.0)
        } else {
            None
        };
        if two {
            let s3 = if pk >= IDLE_BPS && age > 0 {
                f!("peak {} {}", rate(pk, false).s(), ago(age).s())
            } else {
                F::new()
            };
            let (r0, w0) = self.disk_now();
            let s4 = match then {
                Some(t) => f!(
                    " · 1 min ago {}, now {}",
                    rate(t, false).s().trim_end_matches(" MB/s"),
                    rate(r0 + w0, false).s()
                ),
                None => F::new(),
            };
            let s4 = if s3.n + s4.n <= ir.w { s4 } else { F::new() };
            Self::line(scr, ir.x, sy - 1, maxx, bg, &[(s1.s(), th.text)]);
            Self::line(
                scr,
                ir.x,
                sy,
                maxx,
                bg,
                &[(s3.s(), th.text), (s4.s(), th.text)],
            );
        } else if s1.n + s2.n <= ir.w {
            Self::line(
                scr,
                ir.x,
                sy,
                maxx,
                bg,
                &[(s1.s(), th.text), (s2.s(), th.text)],
            );
        } else if pk >= IDLE_BPS {
            let s3 = f!("peak {} {}", rate(pk, false).s(), ago(age).s());
            Self::line(scr, ir.x, sy, maxx, bg, &[(s3.s(), th.text)]);
        } else {
            Self::line(scr, ir.x, sy, maxx, bg, &[(s1.s(), th.text)]);
        }
    }

    fn draw_fs(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let mut k = mixk(&[iw as u64, ih as u64]);
        for (v, f) in self.ma.fs.iter().zip(&self.m.fs) {
            k = mixk(&[
                k,
                v.n.to_bits() as u64,
                fill16(v.x / f.total.max(1) as f32, iw) as u64,
                f.total,
                f.avail,
            ]);
        }
        if !(self.mmemo(K_FS, k) || full) {
            return;
        }
        let th = &self.t;
        let bg = surf(S_PANEL);
        let pct_of = |i: usize| -> f32 {
            let f = &self.m.fs[i];
            let v = self.ma.fs[i];
            // df's use %: used / (used + available), rounded up
            if v.x == v.to {
                f.pct() as f32
            } else {
                (v.x * 100.0 / (v.x + f.avail as f32).max(1.0)).ceil()
            }
        };
        {
            let (p0, fr0, nm) = match self.m.fs.first() {
                Some(f) => (pct_of(0), bsize(f.avail as f32, false), f.mnt.as_str()),
                None => (0.0, F::new(), ""),
            };
            let root = if nm == "/" {
                f!(" system (/) {}% full", p0 as u32)
            } else {
                f!(" {} {}% full", nm, p0 as u32)
            };
            let fr = f!(" · {} free", fr0.s());
            let c = self.hcol(level(p0, FS_WARN, FS_CRIT)) | BOLD;
            let vs: [&[(&str, u32)]; 3] = [
                &[
                    ("storage", th.title | BOLD),
                    (root.s(), c),
                    (fr.s(), th.text),
                ],
                &[("storage", th.title | BOLD), (root.s(), c)],
                &[("storage", th.title | BOLD)],
            ];
            let title = if self.m.fs.is_empty() {
                vs[2]
            } else {
                Self::pick(r.w, &vs)
            };
            let shown = ih.min(self.m.fs.len());
            let more = f!("+ {} more", self.m.fs.len() - shown);
            let sub = if shown < self.m.fs.len() && ih < 3 {
                more.s()
            } else {
                ""
            };
            if full {
                self.panel(scr, r, title, sub);
            } else {
                self.panel_top(scr, r, title);
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let maxx = ir.x + iw;
        let mw = self
            .m
            .fs
            .iter()
            .map(|f| f.mnt.chars().count())
            .max()
            .unwrap_or(1)
            .clamp(1, (iw / 4).max(6))
            + 1;
        let n = self.m.fs.len();
        let per = if ih + 1 >= 3 * n {
            3
        } else if ih >= 2 * n {
            2
        } else {
            1
        };
        let vw = 13;
        let mut y = ir.y;
        for (i, f) in self.m.fs.iter().enumerate() {
            if y >= ir.y + ih {
                break;
            }
            let v = self.ma.fs[i];
            let tot = f.total as f32;
            let fr = v.x / tot.max(1.0);
            let pct = pct_of(i);
            let pl = level(pct, FS_WARN, FS_CRIT);
            let free = f!("{} free", bsize(f.avail as f32, false).s());
            let p = num(" ", pct, 3, "% full");
            let bw = iw.saturating_sub(mw + vw + 1 + p.n);
            let mut x = scr.text(ir.x, y, &f.mnt, th.fg, bg, ir.x + mw - 1);
            while x < ir.x + mw {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
            let fv = rpad(&free, vw);
            x = scr.text(x, y, fv.s(), th.fg, bg, maxx);
            x = scr.text(x, y, " ", 0, bg, maxx);
            if bw >= 4 {
                self.meter(scr, x, y, fr, bw, &th.load, bg, LOAD);
                x += bw;
            }
            Self::line(scr, x, y, maxx, bg, &[(p.s(), self.hcol(pl) | BOLD)]);
            y += 1;
            if per >= 2 {
                let what = if f.mnt == "/" {
                    "  system & home · "
                } else {
                    "  "
                };
                let d = f!(
                    "{}{} used of {} · {} · {}",
                    what,
                    bsize(v.n, false).s(),
                    bsize(tot, false).s(),
                    f.fstype,
                    f.dev
                );
                Self::line(scr, ir.x, y, maxx, bg, &[(d.s(), th.dim)]);
                y += 1;
            }
            if per == 3 && i + 1 < n {
                Self::line(scr, ir.x, y, maxx, bg, &[]);
                y += 1;
            }
            // the last row says how many more there are, if they don't all fit
            if per == 1 && y + 1 == ir.y + ih && i + 2 < n {
                let more = f!("+ {} more", n - i - 1);
                Self::line(scr, ir.x, y, maxx, bg, &[(more.s(), th.dim)]);
                y += 1;
                break;
            }
        }
        while y < ir.y + ih {
            Self::line(scr, ir.x, y, maxx, bg, &[]);
            y += 1;
        }
    }

    fn draw_procs(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let total = (self.m.mi.total as f32).max(1.0);
        let split = self.ma.split;
        let top = self.ma.rows.iter().map(|p| p.mem.x).fold(1.0, f32::max);
        let pw = if split && iw >= 50 { 8 } else { 0 };
        let tail = 9 + 8;
        let bw = iw.saturating_sub(18 + pw + tail).min(24);
        let nw = iw.saturating_sub(pw + bw + tail).min(30);
        let hdr = ih >= 5;
        let nv = ih.saturating_sub(hdr as usize).min(64);
        // rows by place: the k-th row by position takes slot k (so a slot is never
        // empty or doubled); rows on the move trade places where they cross, dimmed
        // by their distance from the slot; rows that left fade out after the others
        let mut slot = [(f32::MAX, usize::MAX); 64];
        let mut order: Vec<usize> = (0..self.ma.rows.len()).collect();
        let rows = &self.ma.rows;
        let key = |i: usize| rows[i].pos.x + if rows[i].src == usize::MAX { 1e6 } else { 0.0 };
        crate::sys::isort(&mut order, &|&a, &b| key(a) < key(b));
        for (k, &i) in order.iter().enumerate().take(nv) {
            slot[k] = ((rows[i].pos.x - k as f32).abs().min(0.5), i);
        }
        let alpha =
            |p: &PRow, d: f32| -> f32 { (p.fade.x * (1.0 - 1.2 * d.min(0.5))).clamp(0.0, 1.0) };
        let mut k = mixk(&[
            iw as u64,
            ih as u64,
            self.m.proc_ok as u64,
            split as u64,
            self.speed as u64,
        ]);
        for &(d, i) in slot.iter().take(nv) {
            if i == usize::MAX {
                k = mixk(&[k, 7]);
                continue;
            }
            let p = &self.ma.rows[i];
            k = mixk(&[
                k,
                p.key,
                p.mem.n.to_bits() as u64,
                fill16(p.mem.x / top, bw) as u64,
                (alpha(p, d) * 12.0).round() as u64,
                p.src as u64,
            ]);
        }
        if !(self.mmemo(K_PROC, k) || full) {
            return;
        }
        let th = &self.t;
        if full {
            let what = if split {
                "top processes"
            } else {
                "top programs"
            };
            let title: [(&str, u32); 2] = [
                (what, th.title | BOLD),
                (" by memory (RAM in use now)", th.mid),
            ];
            let title2: [(&str, u32); 2] = [(what, th.title | BOLD), (" by memory", th.mid)];
            let t: &[(&str, u32)] = if Self::spans_len(&title) + 4 <= r.w {
                &title
            } else {
                &title2
            };
            let eco = self.speed == 0;
            let sub = match (eco, iw >= 44, split) {
                (true, true, false) => "updates every 10 s · p: show each process",
                (true, true, true) => "updates every 10 s · p: group by program",
                (false, true, false) => "● live · p: show each process",
                (false, true, true) => "● live · p: group by program",
                (true, false, _) => "every 10 s",
                (false, false, _) => "● live",
            };
            self.panel(scr, r, t, sub);
        }
        if ih == 0 || nw < 8 {
            return;
        }
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        let mut y = ir.y;
        if !self.m.proc_ok {
            Self::line(scr, ir.x, y, maxx, bg, &[("scanning…", th.dim)]);
            return;
        }
        if hdr {
            let lab = if split {
                "process"
            } else if nw + pw >= 30 {
                "program (×N: N processes)"
            } else {
                "program"
            };
            let mut x = scr.text(ir.x, y, lab, th.dim, bg, maxx);
            let cols: [(usize, &str); 3] = [
                (
                    ir.x + nw + pw.saturating_sub(4),
                    if pw > 0 { "pid" } else { "" },
                ),
                (ir.x + nw + pw + bw + 9 - 6, "in use"),
                (ir.x + nw + pw + bw + 9 + 8 - 5, "% RAM"),
            ];
            for (cx, s) in cols {
                while x < cx {
                    scr.put(x, y, ' ', 0, bg);
                    x += 1;
                }
                x = scr.text(x, y, s, th.dim, bg, maxx);
            }
            Self::line(scr, x, y, maxx, bg, &[]);
            y += 1;
        }
        let panel = th.panel;
        for &(d, i) in slot.iter().take(nv) {
            if i == usize::MAX {
                Self::line(scr, ir.x, y, maxx, bg, &[]);
                y += 1;
                continue;
            }
            let p = self.ma.rows[i];
            let al = alpha(&p, d);
            let fade = |c: u32| mix_lin(panel, c, al);
            let (name, n, pid) = if split {
                match self.m.procs.get(p.src) {
                    Some(q) => (
                        self.m.groups.get(q.group).map_or("?", |g| g.name.as_str()),
                        1,
                        q.pid,
                    ),
                    None => ("", 1, 0),
                }
            } else {
                match self.m.groups.get(p.src) {
                    Some(g) => (g.name.as_str(), g.n, 0),
                    None => ("", 1, 0),
                }
            };
            let cnt = if n > 1 { f!(" ×{}", n) } else { F::new() };
            let keep = nw.saturating_sub(cnt.n + 1);
            // (cut: the last kept letter becomes "…", the count stays)
            let cut = name.chars().count() > keep && keep >= 4;
            let nm = &name[..name.char_indices().nth(if cut { keep - 1 } else { keep }).map_or(name.len(), |c| c.0)];
            let nmf = if cut { fc(&[nm, "…"]) } else { fc(&[nm]) };
            let nm = nmf.s();
            let first = p.pos.to < 0.5;
            let mut x = Self::spans(
                scr,
                ir.x,
                y,
                ir.x + nw - 1,
                bg,
                &[
                    (nm, fade(if first { th.fg } else { th.text })),
                    (cnt.s(), fade(th.mid)),
                ],
            );
            while x < ir.x + nw {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
            if pw > 0 {
                let pd = num("", pid as f32, pw - 1, " ");
                x = scr.text(x, y, pd.s(), fade(th.dim), bg, maxx);
            }
            self.meter(scr, x, y, p.mem.x / top, bw, &th.freq, bg, LOAD);
            x += bw;
            let rs = rpad(&bsize(p.mem.n, false), 9);
            let pc = rpad(&pct1(p.mem.n / total), 8);
            Self::line(
                scr,
                x,
                y,
                maxx,
                bg,
                &[(rs.s(), fade(th.fg)), (pc.s(), fade(th.mid))],
            );
            y += 1;
        }
        while y < ir.y + ih {
            Self::line(scr, ir.x, y, maxx, bg, &[]);
            y += 1;
        }
    }
}

/// A row key for a program name (FNV-1a).
fn name_key(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h & !(1 << 40)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting() {
        assert_eq!(bsize(4.654e9, false).s(), "4.65 GB");
        assert_eq!(bsize(29.95e9, false).s(), "30.0 GB");
        assert_eq!(bsize(229e6, false).s(), "229 MB");
        assert_eq!(bsize(999.6e6, false).s(), "1.00 GB");
        assert_eq!(bsize(512.0, false).s(), "512 B");
        assert_eq!(bsize(0.0, true).s(), "0B");
        assert_eq!(bsize(1.5e9, true).s(), "1.50G");
        assert_eq!(rate(12.34e6, false).s(), "12 MB/s");
        assert_eq!(rate(9.34e6, false).s(), "9.3 MB/s");
        assert_eq!(of_size(5.5e9, 32.16e9).s(), "5.50 of 32.2 GB");
        assert_eq!(of_size(512e6, 32.16e9).s(), "512 MB of 32.2 GB");
        assert!((bstep(4.65e9) - 0.01e9).abs() < 1.0);
        assert_eq!(bstep(500.0), 1.0);
        assert_eq!(pct1(0.0412).s(), "4.1%");
        assert_eq!(rpad(&bsize(1.0, false), 6).s(), "   1 B");
        assert_eq!(ago(58).s(), "58 s ago");
        assert_eq!(ago(120).s(), "2 min ago");
        assert_eq!(ago(90).s(), "1 min 30 s ago");
        assert_eq!(dur(150).s(), "2 min 30 s");
    }

    #[test]
    fn rate_scales_are_round_and_readable() {
        assert_eq!(nice_rate(0.0), 10e6);
        assert_eq!(nice_rate(3e6), 10e6);
        assert_eq!(nice_rate(12e6), 20e6);
        assert_eq!(nice_rate(25e6), 30e6);
        assert_eq!(nice_rate(41e6), 50e6);
        assert_eq!(nice_rate(60e6), 100e6);
        assert_eq!(nice_rate(2.54e9), 3e9);
        for v in [1e6f32, 7e6, 33e6, 180e6, 900e6, 1.2e9, 6e9] {
            let s = nice_rate(v);
            assert!(s >= v && s <= 2.0 * v.max(10e6) + 1.0, "{v} {s}");
        }
        assert_eq!(rate_label(3e9).s(), "3000 MB/s");
        assert_eq!(rate_label(1.5e9).s(), "1500 MB/s");
        assert_eq!(rate_label(5e6).s(), "5 MB/s");
        assert_eq!(rate(2.54e9, false).s(), "2540 MB/s");
        assert_eq!(rate(4e5, false).s(), "0.4 MB/s");
        assert_eq!(rate(1e3, false).s(), "0 MB/s");
        assert_eq!(level(0.17, RAM_WARN, RAM_CRIT), 0);
        assert_eq!(level(0.85, RAM_WARN, RAM_CRIT), 1);
        assert_eq!(level(96.0, FS_WARN, FS_CRIT), 2);
    }
}

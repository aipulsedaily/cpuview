//! Memory & Storage: RAM (stacked bar, legend, DIMM temperatures, history), swap
//! (usage, in / out rates, memory pressure), disk I/O per physical disk (rates, IOPS,
//! busy, temperature, read and write history), filesystems and the biggest processes.
//! Shown as its own page (`m`) or, on big screens, as a column beside the CPU view.

use super::*;
use crate::anim::{Val, W_LOAD, W_TEMP};
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
    /// disk graph width in cells (the rate scale covers what it shows)
    pub gw: usize,
}

impl MemLay {
    pub fn rects(&self) -> [Rect; 4] {
        [self.mem, self.disk, self.fs, self.procs]
    }
}

/// Animated values of the memory area (see `anim::Val`).
#[derive(Default)]
pub struct MA {
    pub(super) used: Val,
    pub(super) cache: Val,
    pub(super) avail: Val,
    pub(super) free: Val,
    pub(super) shared: Val,
    pub(super) swap: Val,
    pub(super) swin: Val,
    pub(super) swout: Val,
    pub(super) psi: Val,
    pub(super) dimm: Vec<Val>,
    /// per disk: read, write (bytes/s), read / write IOPS, busy, temperature, read
    /// and write graph scales
    pub(super) disk: Vec<[Val; 8]>,
    pub(super) fs: Vec<Val>,
    pub(super) procs: Vec<(u32, Val)>,
}

const D_RD: usize = 0;
const D_WR: usize = 1;
const D_RI: usize = 2;
const D_WI: usize = 3;
const D_BUSY: usize = 4;
const D_TEMP: usize = 5;
const D_SR: usize = 6;
const D_SW: usize = 7;

/// memo slots (App::mcache)
const K_MEM: usize = 0;
const K_MGRAPH: usize = 1;
const K_DISK: usize = 2;
const K_DGRAPH: usize = 3;
const K_FS: usize = 4;
const K_PROC: usize = 5;
const K_MTITLE: usize = 6;
const K_DTITLE: usize = 7;

/// Size with 3 significant digits and a binary unit: "4.65 GiB", "229 MiB" (short:
/// "4.65G"). No float formatting.
pub fn bsize(v: f32, short: bool) -> F {
    let (v, k) = unit_of(v);
    let mut f = F::new();
    let c = |m: f32| (v * m).round() as u32;
    if k == 0 || v >= 99.95 {
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
    if short {
        f.push(b"BKMGTP"[k]);
    } else {
        f.push(b' ');
        f.str(["B", "KiB", "MiB", "GiB", "TiB", "PiB"][k]);
    }
    f
}

/// (value in its unit, unit index): below 999.5 of the unit.
fn unit_of(v: f32) -> (f32, usize) {
    let (mut v, mut k) = (v.max(0.0), 0);
    while v >= 999.5 && k < 5 {
        v /= 1024.0;
        k += 1;
    }
    (v, k)
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
    s * (1u64 << (10 * k)) as f32
}

pub fn rate(v: f32, short: bool) -> F {
    let mut f = bsize(v, short);
    f.str("/s");
    f
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

/// Graph scale for a rate peak: a power of two, 1 MiB/s at least.
pub fn nice(v: f32) -> f32 {
    let mut s = 1_048_576.0f32;
    while s < v && s < 1e15 {
        s *= 2.0;
    }
    s
}

/// "64M": a power-of-two scale as an axis label.
pub fn blabel(v: f32) -> F {
    let (mut v, mut k) = (v, 0);
    while v >= 1024.0 && k < 5 {
        v /= 1024.0;
        k += 1;
    }
    let mut f = F::new();
    f.int(v.round() as u32, 0);
    f.push(b"BKMGTP"[k]);
    f
}

impl App {
    /// The memory area is on screen (its own page, or beside the CPU view).
    pub fn mem_visible(&self) -> bool {
        self.ml.mem.w > 0
    }

    pub fn procs_visible(&self) -> bool {
        self.ml.procs.w > 0
    }

    // ── layout ─────────────────────────────────────────────────────────────

    /// Rows of statistics the memory panel wants at inner width `iw`.
    fn mem_rows(&self, iw: usize) -> (usize, usize, usize) {
        let items = 5 + !self.m.dimms.is_empty() as usize;
        let ncol = if iw >= 46 { 2 } else { 1 };
        let swap = if self.m.mi.swap_total > 0 || self.m.has_psi {
            2
        } else {
            1
        };
        (items.div_ceil(ncol), swap, ncol)
    }

    fn disk_rows(&self, iw: usize) -> usize {
        let nd = self.m.disks.len();
        nd * (if iw >= 70 { 2 } else { 3 } + (nd > 1) as usize)
    }

    /// Panels in `r`: two columns (memory over storage | disks over processes) or
    /// one, by priority: memory, disks, storage, processes; then statistics, then
    /// graphs and more processes.
    pub fn mem_layout(&self, r: Rect, two: bool) -> MemLay {
        let m = &self.m;
        let nd = m.disks.len();
        let nfs = m.fs.len();
        let np = crate::mem::NPROC;
        let mut l = MemLay::default();
        let at = |x: usize, y: usize, w: usize, h: usize| Rect { x, y, w, h };
        if two {
            let (lw, rw) = (r.w / 2, r.w - r.w / 2);
            let (lr, sw, _) = self.mem_rows(lw - 4);
            let top_min = (3 + lr + sw).max(2 + self.disk_rows(rw - 4)) + 4;
            let fs_need = 2 + nfs.max(1);
            let b = fs_need
                .max((r.h * 36 / 100).min(3 + np))
                .min(r.h.saturating_sub(top_min))
                .max(fs_need.min(r.h / 2));
            let t = r.h - b;
            l.mem = at(r.x, r.y, lw, t);
            l.fs = at(r.x, r.y + t, lw, b);
            if nd > 0 {
                l.disk = at(r.x + lw, r.y, rw, t);
            }
            if nd == 0 {
                l.procs = at(r.x + lw, r.y, rw, r.h);
            } else {
                l.procs = at(r.x + lw, r.y + t, rw, b);
            }
            if nfs == 0 {
                l.mem.h = r.h;
                l.fs = Rect::default();
            }
            l.gw = rw - 4;
        } else {
            let iw = r.w.saturating_sub(4);
            let (lr, sw, _) = self.mem_rows(iw);
            // (min, statistics, weight of the rest)
            let pans: [(usize, usize, usize); 4] = [
                (3, 3 + lr + sw + 1, 5),
                (
                    if nd > 0 { 2 + nd } else { 0 },
                    if nd > 0 { 2 + self.disk_rows(iw) } else { 0 },
                    4,
                ),
                (
                    if nfs > 0 { 2 + nfs } else { 0 },
                    if nfs > 0 { 2 + nfs } else { 0 },
                    0,
                ),
                (if r.w >= 34 { 6 } else { 0 }, 3 + np, 0),
            ];
            let mut hs = [0usize; 4];
            let mut left = r.h;
            for (k, p) in pans.iter().enumerate() {
                let need = p.0.min(if k == 2 { left } else { usize::MAX });
                if need > 0 && need <= left && (k != 2 || need >= 3) {
                    hs[k] = need;
                    left -= need;
                }
            }
            if hs[0] == 0 && r.h >= 3 {
                hs[0] = r.h.min(3);
                left = r.h - hs[0];
            }
            for k in [1, 0, 2] {
                // (the disk rows are all or one line per disk)
                if hs[k] > 0 && (k != 1 || pans[1].1 - hs[1] <= left) {
                    let g = pans[k].1.saturating_sub(hs[k]).min(left);
                    hs[k] += g;
                    left -= g;
                }
            }
            // processes get a third of the rest (up to their list), graphs the others
            if hs[3] > 0 {
                let g = (left / 3).min(pans[3].1.saturating_sub(hs[3]));
                hs[3] += g;
                left -= g;
            }
            let a = if hs[1] > 0 { left * 5 / 9 } else { left };
            hs[0] += a;
            if hs[1] > 0 {
                hs[1] += left - a;
            } else if hs[3] > 0 {
                hs[3] += left - a;
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
        a.free.set(f(mi.free), smooth);
        a.shared.set(f(mi.shmem), smooth);
        a.swap.set(f(mi.swap_used()), smooth);
        a.swin.set(m.swin, smooth);
        a.swout.set(m.swout, smooth);
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
        let pts = 2 * self.ml.gw.max(1);
        for (k, (v, d)) in a.disk.iter_mut().zip(&m.disks).enumerate() {
            let sm = smooth && k < n0;
            let peak = |h: &crate::sys::Ring<f32>, cur: f32| {
                (0..h.len().min(pts)).map(|i| h.back(i)).fold(cur, f32::max)
            };
            let t = d.temp.and_then(slow_temp).unwrap_or(0.0);
            for (j, x) in [
                d.rd,
                d.wr,
                d.ri,
                d.wi,
                d.busy,
                t,
                nice(peak(&d.h_rd, d.rd)),
                nice(peak(&d.h_wr, d.wr)),
            ]
            .into_iter()
            .enumerate()
            {
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
        let old = std::mem::take(&mut a.procs);
        for p in &m.procs {
            let mut v = old.iter().find(|o| o.0 == p.pid).map(|o| o.1);
            match &mut v {
                Some(v) => v.set(f(p.rss), smooth),
                None => {
                    let mut n = Val::default();
                    fresh(&mut n, f(p.rss), smooth);
                    v = Some(n);
                }
            }
            a.procs.push((p.pid, v.unwrap()));
        }
    }

    /// Smooth mode: one frame of every memory-area spring and digit. Returns whether
    /// anything moves or changed.
    pub fn mem_step(&mut self, dt: f32) -> bool {
        let (xl, xt) = ((-W_LOAD * dt).exp(), (-W_TEMP * dt).exp());
        let total = self.m.mi.total as f32;
        let swt = self.m.mi.swap_total as f32;
        let a = &mut self.ma;
        let mut mv = false;
        let by =
            |v: &mut Val, full: f32| v.step(dt, W_LOAD, xl, 1.0) | v.digits(dt, bstep(v.x), full);
        mv |= by(&mut a.used, total);
        mv |= by(&mut a.cache, total);
        mv |= by(&mut a.avail, total);
        mv |= by(&mut a.free, total);
        mv |= by(&mut a.shared, total);
        mv |= by(&mut a.swap, swt);
        mv |= by(&mut a.swin, 0.0);
        mv |= by(&mut a.swout, 0.0);
        for v in a.fs.iter_mut() {
            mv |= by(v, 0.0);
        }
        for (_, v) in a.procs.iter_mut() {
            mv |= by(v, 0.0);
        }
        for d in a.disk.iter_mut() {
            let (sr, sw) = (d[D_SR].x, d[D_SW].x);
            mv |= by(&mut d[D_RD], sr);
            mv |= by(&mut d[D_WR], sw);
            for j in [D_SR, D_SW] {
                mv |= d[j].step(dt, W_LOAD, xl, 1.0);
            }
            for j in [D_RI, D_WI] {
                mv |= d[j].step(dt, W_LOAD, xl, 0.01);
                mv |= d[j].digits(dt, 1.0, 0.0);
            }
            mv |= d[D_BUSY].step(dt, W_LOAD, xl, 0.0005);
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

    /// History series for `n` graph columns' worth of points (2 per cell): the points
    /// as they are, or in smooth mode the scrolling curve with the live value at its head.
    fn series(
        &self,
        out: &mut Vec<f32>,
        n: usize,
        h: &crate::sys::Ring<f32>,
        head: f32,
        part: f32,
    ) {
        if !self.smooth() {
            h.tail(n, 0.0, out);
            return;
        }
        crate::anim::Curve {
            phase: self.phase,
            head,
            part,
            len: h.len(),
            pt: &|k| h.back(k),
            pad: 0.0,
        }
        .sample(out, n, 0.0, 1.0);
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

    /// RAM bar colours: used (by position, as the load bars), cache (flat).
    fn cache_col(&self) -> u32 {
        self.t.freq_idle.at(0.45)
    }

    /// Stacked bar: used up to `a`, cache up to `b` (fractions), free track after.
    #[allow(clippy::too_many_arguments)]
    fn stack_bar(&self, scr: &mut Screen, x: usize, y: usize, w: usize, a: f32, b: f32, bg: u32) {
        let (na, nb) = (fill16(a, w), fill16(b.max(a), w));
        let den = (w.max(2) - 1) as f32;
        let cc = self.cache_col();
        for i in 0..w {
            let uc = self.t.load.at(i as f32 / den);
            let fg = if i < na / 16 {
                uc
            } else if i == na / 16 && na % 16 > 0 {
                if nb >= (i + 1) * 16 {
                    mix_lin(cc, uc, (na % 16) as f32 / 16.0)
                } else {
                    uc | ((na % 16) as u32) << BLEND_SHIFT
                }
            } else if i < nb / 16 {
                cc
            } else if i == nb / 16 && nb % 16 > 0 {
                cc | ((nb % 16) as u32) << BLEND_SHIFT
            } else {
                TRACK
            };
            scr.put(x + i, y, LOAD, fg, bg);
        }
    }

    fn draw_ram(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let total = (self.m.mi.total as f32).max(1.0);
        let a = &self.ma;
        let (used, cache, un) = (a.used.x, a.cache.x.min(total - a.used.x).max(0.0), a.used.n);
        let fu = used / total;
        let (lr_all, sw_all, ncol) = self.mem_rows(iw);
        // rows by priority: bar, legend, swap, legend, swap, legend...
        let mut rem = ih.saturating_sub(1);
        let (mut lr, mut sr) = (0, 0);
        for k in 0..lr_all + sw_all {
            if rem == 0 {
                break;
            }
            if k % 2 == 1 && sr < sw_all || lr >= lr_all {
                if sr < sw_all {
                    sr += 1;
                    rem -= 1;
                }
            } else {
                lr += 1;
                rem -= 1;
            }
        }
        let gap1 = (rem >= 2 && sr > 0) as usize;
        rem -= gap1;
        let gh = if rem >= 5 { rem - 1 } else { 0 };
        let tk = mixk(&[a.used.n.to_bits() as u64, iw as u64]);
        if self.mmemo(K_MTITLE, tk) || full {
            let th = &self.t;
            let p = num(" ", un / total * 100.0, 0, "%");
            let (u, t) = (bsize(un, false), bsize(total, false));
            let ut = f!("  {} of {}", u.s(), t.s());
            let c = th.load.at(fu) | BOLD;
            let vs: [&[(&str, u32)]; 3] = [
                &[("memory", th.title | BOLD), (p.s(), c), (ut.s(), th.mid)],
                &[("memory", th.title | BOLD), (p.s(), c)],
                &[("mem", th.title | BOLD)],
            ];
            let title = Self::pick(r.w, &vs);
            if full {
                let win = span(2 * iw.saturating_sub(5));
                let sub = if gh > 0 {
                    f!("used · cache · last {}", win.s())
                } else {
                    F::new()
                };
                self.panel(scr, r, title, sub.s());
            } else {
                self.panel_top(scr, r, title);
            }
        }
        if ih == 0 || iw < 6 {
            return;
        }
        let a = &self.ma;
        let mut k = mixk(&[
            fill16(fu, iw) as u64,
            fill16((used + cache) / total, iw) as u64,
            iw as u64,
            ih as u64,
        ]);
        for v in [
            &a.used, &a.cache, &a.avail, &a.free, &a.shared, &a.swap, &a.swin, &a.swout, &a.psi,
        ] {
            k = mixk(&[k, v.n.to_bits() as u64]);
        }
        let swt = self.m.mi.swap_total as f32;
        k = mixk(&[k, fill16(a.swap.x / swt.max(1.0), iw) as u64]);
        for d in &a.dimm {
            k = mixk(&[k, d.n.to_bits() as u64]);
        }
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        if self.mmemo(K_MEM, k) || full {
            let th = &self.t;
            let a = &self.ma;
            let mut y = ir.y;
            self.stack_bar(scr, ir.x, y, iw, fu, (used + cache) / total, bg);
            y += 1;
            // legend
            let cw = if ncol == 2 { (iw + 2) / 2 } else { iw };
            let short = cw < 22;
            let dimms = {
                let mut f = F::new();
                for (i, d) in a.dimm.iter().enumerate() {
                    if i > 0 {
                        f.push(b' ');
                    }
                    if d.n > 0.0 {
                        f.str(num("", d.n, 0, "°").s());
                    } else {
                        f.str("–");
                    }
                }
                f
            };
            let dt = a.dimm.iter().map(|d| d.x).fold(0.0, f32::max);
            let items: [(u32, &str, F, F); 6] = [
                (
                    th.load.at(fu.max(0.15)),
                    "used",
                    bsize(a.used.n, short),
                    num(" ", a.used.n / total * 100.0, 3, "%"),
                ),
                (self.cache_col(), "cache", bsize(a.cache.n, short), F::new()),
                (
                    0,
                    "avail",
                    bsize(a.avail.n, short),
                    num(" ", a.avail.n / total * 100.0, 3, "%"),
                ),
                (0, "free", bsize(a.free.n, short), F::new()),
                (0, "shared", bsize(a.shared.n, short), F::new()),
                (0, "dimm", dimms, F::new()),
            ];
            let nitems = 5 + !a.dimm.is_empty() as usize;
            for row in 0..lr {
                for c in 0..ncol {
                    let i = row * ncol + c;
                    let x = ir.x + c * cw;
                    let mx = if c + 1 == ncol { maxx } else { x + cw };
                    if i >= nitems {
                        Self::line(scr, x, y, mx, bg, &[]);
                        continue;
                    }
                    let (sc, lab, v, e) = &items[i];
                    let (sw, sc) = if *sc != 0 { ("■ ", *sc) } else { ("  ", 0) };
                    let lab = f!("{:<7}", lab);
                    let vw = rpad(v, if short { 5 } else { 8 });
                    let vc = if i == 5 {
                        th.temp.at((dt - 30.0) / 55.0)
                    } else {
                        th.text
                    };
                    let lab = if short {
                        f!("{:<6}", lab.s().trim_end())
                    } else {
                        lab
                    };
                    Self::line(
                        scr,
                        x,
                        y,
                        mx,
                        bg,
                        &[
                            (sw, sc),
                            (lab.s(), th.dim),
                            (if i == 5 { v.s() } else { vw.s() }, vc),
                            (e.s(), th.mid),
                        ],
                    );
                }
                y += 1;
            }
            if gap1 > 0 {
                Self::line(scr, ir.x, y, maxx, bg, &[]);
                y += 1;
            }
            let mi = self.m.mi;
            if sr > 0 {
                if mi.swap_total > 0 {
                    let short = iw < 44;
                    let u = bsize(a.swap.n, short);
                    let t = bsize(swt, short);
                    let ut = if short {
                        f!(" {}", u.s())
                    } else {
                        f!(" {} / {}", u.s(), t.s())
                    };
                    let p = num(" ", a.swap.n / swt * 100.0, 3, "%");
                    let bw = iw.saturating_sub(5 + ut.n + p.n);
                    scr.text(ir.x, y, "swap ", th.dim, bg, maxx);
                    let fs = a.swap.x / swt;
                    self.meter(scr, ir.x + 5, y, fs, bw, &th.load, bg, LOAD);
                    Self::line(
                        scr,
                        ir.x + 5 + bw,
                        y,
                        maxx,
                        bg,
                        &[(ut.s(), th.text), (p.s(), th.load.at(fs))],
                    );
                } else {
                    Self::line(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        bg,
                        &[("swap ", th.dim), ("none", th.mid)],
                    );
                }
                y += 1;
            }
            if sr > 1 {
                let short = iw < 44;
                let (i, o) = (rate(a.swin.n, short), rate(a.swout.n, short));
                let ps = pct1(a.psi.n / 100.0);
                let sc = |v: f32| if v > 0.0 { th.warn } else { th.text };
                let pc = if a.psi.n >= 1.0 { th.warn } else { th.text };
                let sw_on = mi.swap_total > 0;
                let full_v: &[(&str, u32)] = &[
                    ("  in ", th.dim),
                    (i.s(), sc(a.swin.n)),
                    ("   out ", th.dim),
                    (o.s(), sc(a.swout.n)),
                    (if self.m.has_psi { "   pressure " } else { "" }, th.dim),
                    (if self.m.has_psi { ps.s() } else { "" }, pc),
                ];
                let mid_v: &[(&str, u32)] = &[
                    ("  in ", th.dim),
                    (i.s(), sc(a.swin.n)),
                    (" out ", th.dim),
                    (o.s(), sc(a.swout.n)),
                ];
                let psi_v: &[(&str, u32)] = &[("pressure ", th.dim), (ps.s(), pc)];
                let sp = if !sw_on {
                    psi_v
                } else if Self::spans_len(full_v) <= iw {
                    full_v
                } else {
                    mid_v
                };
                Self::line(scr, ir.x, y, maxx, bg, sp);
                y += 1;
            }
            if gh > 0 {
                Self::line(scr, ir.x, y, maxx, bg, &[]);
            }
        }
        if gh > 0 && (graphs || full) {
            let gy = ir.y + ih - gh;
            let body = self.body_key(2);
            if self.mmemo(K_MGRAPH, mixk(&[body, gh as u64, iw as u64])) || full {
                let mut vals = std::mem::take(&mut self.vals);
                let mut v2 = std::mem::take(&mut self.vals2);
                let m = &self.m;
                let cur = m.mi.used() as f32 / total;
                let cc = (m.mi.cache() as f32 / total).min(1.0 - cur);
                self.series(&mut vals, 2 * iw, &m.h_used, fu, cur);
                self.series(
                    &mut v2,
                    2 * iw,
                    &m.h_cache,
                    (used + cache) / total,
                    cur + cc,
                );
                self.vals2 = v2;
                self.under.set(self.cache_col());
                self.axis_graph(
                    scr,
                    ir.x,
                    gy,
                    iw,
                    gh,
                    &vals,
                    1.0,
                    &self.t.load,
                    &[(1.0, "100%"), (0.5, "50"), (0.0, "0")],
                );
                self.under.set(0);
                self.vals = vals;
            }
        }
    }

    fn draw_disks(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let nd = self.m.disks.len();
        let a = &self.ma;
        let one = nd == 1;
        let two = iw >= 70;
        let per = if two { 2 } else { 3 } + (!one) as usize;
        let compact = ih < nd * per;
        let gh_all = if compact { 0 } else { ih - nd * per };
        let g_each = gh_all / nd.max(1);
        let tk = if one {
            a.disk[0][D_TEMP].n.to_bits() as u64
        } else {
            nd as u64
        };
        if self.mmemo(K_DTITLE, mixk(&[tk, iw as u64])) || full {
            let th = &self.t;
            let d = &self.m.disks[0];
            let t = self.ma.disk[0][D_TEMP].n;
            let tt = num("  ", t, 0, "°C");
            let tc = th.temp.at((t - 30.0) / 55.0);
            let tsp = (if t > 0.0 { tt.s() } else { "" }, tc);
            let model = f!(" · {}", d.model);
            let sz = f!(" · {}", bsize(d.size as f32, false).s());
            let nn = f!(" {}", nd);
            let vs1: [&[(&str, u32)]; 4] = [
                &[
                    ("disk ", th.title | BOLD),
                    (&d.name, th.text),
                    (model.s(), th.mid),
                    (sz.s(), th.dim),
                    tsp,
                ],
                &[
                    ("disk ", th.title | BOLD),
                    (&d.name, th.text),
                    (model.s(), th.mid),
                    tsp,
                ],
                &[("disk ", th.title | BOLD), (&d.name, th.text), tsp],
                &[("disk", th.title | BOLD)],
            ];
            let vsn: [&[(&str, u32)]; 1] = [&[("disks", th.title | BOLD), (nn.s(), th.text)]];
            let title = if one { Self::pick(r.w, &vs1) } else { vsn[0] };
            if full {
                let win = span(2 * iw.saturating_sub(6));
                let sub = if g_each >= 4 {
                    f!("read · write · last {}", win.s())
                } else {
                    F::new()
                };
                self.panel(scr, r, title, sub.s());
            } else {
                self.panel_top(scr, r, title);
            }
        }
        if ih == 0 || iw < 8 {
            return;
        }
        let a = &self.ma;
        let mut k = mixk(&[iw as u64, ih as u64]);
        for d in &a.disk {
            for v in &d[..D_SR] {
                k = mixk(&[k, v.n.to_bits() as u64]);
            }
            k = mixk(&[k, fill16(d[D_BUSY].x, iw) as u64]);
        }
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        let (rc, wc) = (self.t.freq.at(0.55), self.t.load.at(0.05));
        if self.mmemo(K_DISK, k) || full {
            let th = &self.t;
            let mut y = ir.y;
            for (i, d) in self.m.disks.iter().enumerate() {
                if y >= ir.y + ih {
                    break;
                }
                let v = &self.ma.disk[i];
                let short = iw < 40;
                let (rr, wr) = (rate(v[D_RD].n, short), rate(v[D_WR].n, short));
                let (ri, wi) = (num("", v[D_RI].n, 0, ""), num("", v[D_WI].n, 0, ""));
                let bp = num(" ", v[D_BUSY].n * 100.0, 3, "%");
                if compact {
                    let nm = if one { F::new() } else { f!("{:<8}", d.name) };
                    Self::line(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        bg,
                        &[
                            (nm.s(), th.text),
                            ("R ", th.dim),
                            (rpad(&rr, if short { 6 } else { 10 }).s(), rc),
                            ("  W ", th.dim),
                            (rpad(&wr, if short { 6 } else { 10 }).s(), wc),
                            (bp.s(), th.load.at(v[D_BUSY].x)),
                        ],
                    );
                    y += 1;
                    continue;
                }
                if !one {
                    let t = v[D_TEMP].n;
                    let tt = num("  ", t, 0, "°C");
                    let md = f!("  {}", d.model);
                    Self::line(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        bg,
                        &[
                            (&d.name, th.fg | BOLD),
                            (md.s(), th.mid),
                            (
                                if t > 0.0 { tt.s() } else { "" },
                                th.temp.at((t - 30.0) / 55.0),
                            ),
                        ],
                    );
                    y += 1;
                }
                let cell = |lab: &str,
                            b: &F,
                            io: &F,
                            c: u32,
                            scr: &mut Screen,
                            x: usize,
                            y: usize,
                            mx: usize| {
                    let l = f!("{:<6}", lab);
                    let bb = rpad(b, if short { 7 } else { 10 });
                    let ii = rpad(io, 8);
                    Self::line(
                        scr,
                        x,
                        y,
                        mx,
                        bg,
                        &[
                            (l.s(), c),
                            (bb.s(), th.fg),
                            (ii.s(), th.mid),
                            (" IOPS", th.dim),
                        ],
                    );
                };
                if two {
                    let h = ir.x + iw.div_ceil(2);
                    cell("read", &rr, &ri, rc, scr, ir.x, y, h);
                    cell("write", &wr, &wi, wc, scr, h, y, maxx);
                    y += 1;
                } else {
                    cell("read", &rr, &ri, rc, scr, ir.x, y, maxx);
                    cell("write", &wr, &wi, wc, scr, ir.x, y + 1, maxx);
                    y += 2;
                }
                let bw = iw.saturating_sub(6 + 5);
                scr.text(ir.x, y, "busy  ", th.dim, bg, maxx);
                self.meter(scr, ir.x + 6, y, v[D_BUSY].x, bw, &th.load, bg, LOAD);
                Self::line(
                    scr,
                    ir.x + 6 + bw,
                    y,
                    maxx,
                    bg,
                    &[(bp.s(), th.load.at(v[D_BUSY].x))],
                );
                y += 1 + g_each;
            }
        }
        if g_each >= 4 && (graphs || full) {
            let body = self.body_key(0);
            let mut sk = mixk(&[body, iw as u64, ih as u64]);
            for d in &self.ma.disk {
                sk = mixk(&[sk, d[D_SR].to.to_bits() as u64, d[D_SW].to.to_bits() as u64]);
            }
            if self.mmemo(K_DGRAPH, sk) || full {
                let mut vals = std::mem::take(&mut self.vals);
                for i in 0..nd {
                    let y0 = ir.y + (i + 1) * per + i * g_each;
                    let hr = g_each / 2;
                    let d = &self.m.disks[i];
                    let v = self.ma.disk[i];
                    let (pr, pw) = d.part();
                    for (j, (h, gy, gh, head, part, sc, ramp, lab)) in [
                        (
                            &d.h_rd,
                            y0,
                            hr,
                            v[D_RD].x,
                            pr,
                            v[D_SR],
                            &self.t.freq,
                            "read",
                        ),
                        (
                            &d.h_wr,
                            y0 + hr,
                            g_each - hr,
                            v[D_WR].x,
                            pw,
                            v[D_SW],
                            &self.t.load,
                            "write",
                        ),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let _ = j;
                        self.series(&mut vals, 2 * iw, h, head, part);
                        let sl = blabel(sc.to);
                        let l3 = [(1.0, sl.s()), (0.5, lab), (0.0, "0")];
                        let l2 = [(1.0, sl.s()), (0.0, lab)];
                        let labs: &[(f32, &str)] = if gh >= 6 { &l3 } else { &l2 };
                        self.axis_graph(scr, ir.x, gy, iw, gh, &vals, sc.x.max(1.0), ramp, labs);
                    }
                }
                self.vals = vals;
            }
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
            ]);
        }
        if !(self.mmemo(K_FS, k) || full) {
            return;
        }
        if full {
            let nn = f!(" {}", self.m.fs.len());
            let title: [(&str, u32); 2] = [("storage", self.t.title | BOLD), (nn.s(), self.t.text)];
            let sub = if iw >= 40 { "used / size · use %" } else { "" };
            self.panel(scr, r, &title, sub);
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let th = &self.t;
        let bg = surf(S_PANEL);
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
        let tw = if iw >= 64 && per == 1 { 6 } else { 0 };
        let short = iw < 50;
        let fw = if iw >= 96 { 16 } else { 0 };
        // spare rows: a second line per filesystem (type, device, free), then a gap
        let mut y = ir.y;
        for (i, f) in self.m.fs.iter().enumerate() {
            if y >= ir.y + ih {
                break;
            }
            let v = self.ma.fs[i];
            let tot = f.total as f32;
            let fr = v.x / tot.max(1.0);
            // df's use %: used / (used + available), rounded up
            let pct = if v.x == v.to {
                f.pct() as f32
            } else {
                (v.x * 100.0 / (v.x + f.avail as f32).max(1.0)).ceil()
            };
            let ut = if short {
                f!(" {}", bsize(v.n, true).s())
            } else {
                f!(
                    " {} / {}",
                    rpad(&bsize(v.n, false), 8).s(),
                    rpad(&bsize(tot, false), 8).s()
                )
            };
            let p = num(" ", pct, 3, "%");
            let fr_t = f!("  {} free", bsize(f.avail as f32, false).s());
            let fr_t = rpad(&fr_t, fw);
            let bw = iw.saturating_sub(mw + tw + ut.n + p.n + fw);
            let mut x = scr.text(ir.x, y, &f.mnt, th.fg, bg, ir.x + mw - 1);
            while x < ir.x + mw {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
            if tw > 0 {
                let t = f!("{:<6}", f.fstype);
                x = scr.text(x, y, t.s(), th.dim, bg, x + tw);
            }
            self.meter(scr, x, y, fr, bw, &th.load, bg, LOAD);
            Self::line(
                scr,
                x + bw,
                y,
                maxx,
                bg,
                &[
                    (ut.s(), th.text),
                    (p.s(), th.load.at(fr) | BOLD),
                    (if fw > 0 { fr_t.s() } else { "" }, th.mid),
                ],
            );
            y += 1;
            if per >= 2 {
                let d = f!("  {} · {}", f.fstype, f.dev);
                let fr = f!(" · {} free", bsize(f.avail as f32, false).s());
                Self::line(scr, ir.x, y, maxx, bg, &[(d.s(), th.dim), (fr.s(), th.mid)]);
                y += 1;
            }
            if per == 3 && i + 1 < n {
                Self::line(scr, ir.x, y, maxx, bg, &[]);
                y += 1;
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
        let top = self.ma.procs.iter().map(|p| p.1.x).fold(1.0, f32::max);
        let pw = if iw >= 50 { 8 } else { 0 };
        let bw = iw.saturating_sub(16 + pw + 9 + 7).min(24);
        let nw = iw.saturating_sub(pw + bw + 9 + 7).min(28);
        let mut k = mixk(&[iw as u64, ih as u64, self.m.proc_ok as u64]);
        for (pid, v) in &self.ma.procs {
            k = mixk(&[
                k,
                *pid as u64,
                v.n.to_bits() as u64,
                fill16(v.x / top, bw) as u64,
            ]);
        }
        if !(self.mmemo(K_PROC, k) || full) {
            return;
        }
        if full {
            let title: [(&str, u32); 2] = [
                ("processes", self.t.title | BOLD),
                (" by memory", self.t.mid),
            ];
            let sub = if iw >= 36 {
                if self.speed == 0 {
                    "resident · every 20 s"
                } else {
                    "resident · every 3 s"
                }
            } else {
                ""
            };
            self.panel(scr, r, &title, sub);
        }
        if ih == 0 || nw < 8 {
            return;
        }
        let th = &self.t;
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        let mut y = ir.y;
        if !self.m.proc_ok {
            Self::line(scr, ir.x, y, maxx, bg, &[("scanning…", th.dim)]);
            return;
        }
        if ih >= 4 {
            let mut x = scr.text(ir.x, y, "name", th.dim, bg, maxx);
            let cols: [(usize, &str); 3] = [
                (
                    ir.x + nw + pw.saturating_sub(4),
                    if pw > 0 { "pid" } else { "" },
                ),
                (ir.x + nw + pw + bw + 9 - 3, "rss"),
                (ir.x + nw + pw + bw + 9 + 7 - 4, "mem"),
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
        for (i, p) in self.m.procs.iter().enumerate() {
            if y >= ir.y + ih {
                break;
            }
            let v = self.ma.procs.get(i).map_or(Val::default(), |v| v.1);
            let mut x = scr.text(
                ir.x,
                y,
                &p.name,
                if i == 0 { th.fg } else { th.text },
                bg,
                ir.x + nw - 1,
            );
            while x < ir.x + nw {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
            if pw > 0 {
                let pd = num("", p.pid as f32, pw - 1, " ");
                x = scr.text(x, y, pd.s(), th.dim, bg, maxx);
            }
            let fr = v.x / top;
            self.meter(scr, x, y, fr, bw, &th.freq, bg, LOAD);
            x += bw;
            let rs = rpad(&bsize(v.n, false), 9);
            let pc = rpad(&pct1(v.n / total), 7);
            Self::line(scr, x, y, maxx, bg, &[(rs.s(), th.fg), (pc.s(), th.mid)]);
            y += 1;
        }
        while y < ir.y + ih {
            Self::line(scr, ir.x, y, maxx, bg, &[]);
            y += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting() {
        let g = (1u64 << 30) as f32;
        assert_eq!(bsize(4.654 * g, false).s(), "4.65 GiB");
        assert_eq!(bsize(29.95 * g, false).s(), "30.0 GiB");
        assert_eq!(bsize(229.0 * 1048576.0, false).s(), "229 MiB");
        assert_eq!(bsize(999.6 * 1048576.0, false).s(), "0.98 GiB");
        assert_eq!(bsize(512.0, false).s(), "512 B");
        assert_eq!(bsize(0.0, true).s(), "0B");
        assert_eq!(bsize(1.5 * g, true).s(), "1.50G");
        assert_eq!(rate(12.34 * 1048576.0, false).s(), "12.3 MiB/s");
        assert_eq!(bstep(4.65 * g), 0.01 * g);
        assert_eq!(bstep(500.0), 1.0);
        assert_eq!(pct1(0.0412).s(), "4.1%");
        assert_eq!(rpad(&bsize(1.0, false), 6).s(), "   1 B");
        assert_eq!(nice(3e6), 4_194_304.0);
        assert_eq!(nice(0.0), 1_048_576.0);
        assert_eq!(blabel(64.0 * 1048576.0).s(), "64M");
        assert_eq!(blabel(2.0 * g).s(), "2G");
    }
}

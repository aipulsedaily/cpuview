//! View 5: sensors & power (key `5`). Temperatures grouped by part (CPU,
//! motherboard, memory, storage, graphics, network) with plain names, fans with their
//! speed and how the board drives them, CPU / GPU / battery power, voltages, and the
//! drivers all this comes from. Whatever a machine doesn't have doesn't appear; what
//! needs a driver or a permission says how to get it.
//!
//! The memory page's grammar: a plain headline per panel coloured by health, rows of
//! label → value → bar → note (the hardware's own name dim at the end), history charts
//! that say they're over time with a "2 min ago · 1 min ago · now" axis. Rows are
//! redrawn only when they change (an eco sample changes a few).

use super::*;
use crate::anim::{Curve, Val, W_CLOCK, W_LOAD, W_TEMP};
use crate::sensors::*;

/// Sensors-page panels (`w == 0`: not shown).
#[derive(Clone, Copy, Default)]
pub struct SensLay {
    pub temp: Rect,
    pub fan: Rect,
    pub power: Rect,
    pub volt: Rect,
    pub src: Rect,
    sig: u64,
}

impl SensLay {
    pub fn rects(&self) -> [Rect; 5] {
        [self.temp, self.fan, self.power, self.volt, self.src]
    }
}

/// Animated values: one per reading (`Sens::rd`), and the cores' total.
#[derive(Default)]
pub struct ZA {
    pub(super) v: Vec<Val>,
    csum: Val,
    l: ZL,
}

// memo slots (rows: 200 + y)
const Z_TEMP: usize = 0;
const Z_TLAY: usize = 1;
const Z_TGRAPH: usize = 2;
const Z_FAN: usize = 3;
const Z_FBODY: usize = 4;
const Z_FGRAPH: usize = 5;
const Z_POW: usize = 6;
const Z_PBODY: usize = 7;
const Z_PGRAPH: usize = 8;
const Z_VOLT: usize = 9;
const Z_SRC: usize = 10;

/// Fixed-point decimal: `v` with `p` decimals.
fn dec(v: f32, p: u32) -> F {
    let m = 10u32.pow(p);
    let c = (v.abs() * m as f32).round() as u32;
    let mut f = F::new();
    if v < 0.0 && c > 0 {
        f.push(b'-');
    }
    f.int(c / m, 0);
    if p > 0 {
        f.push(b'.');
        let fr = c % m;
        let mut d = m / 10;
        while d > 0 {
            f.push(b'0' + (fr / d % 10) as u8);
            d /= 10;
        }
    }
    f
}

/// "117 W", "43.2 W", "5.62 W".
pub fn watts(v: f32) -> F {
    let v = v.max(0.0);
    if v > 0.0 && v < 0.005 {
        return fc(&["<0.01 W"]);
    }
    let mut f = dec(v, if v >= 99.5 { 0 } else if v >= 9.95 { 1 } else { 2 });
    f.str(" W");
    f
}

/// The resolution `watts` shows `v` with.
fn wstep(v: f32) -> f32 {
    if v >= 99.5 {
        1.0
    } else if v >= 9.95 {
        0.1
    } else {
        0.01
    }
}

/// "49°C".
pub fn deg(v: f32) -> F {
    let mut f = dec(v, 0);
    f.str("°C");
    f
}

fn volts(v: f32) -> F {
    let mut f = dec(v, 2);
    f.str(" V");
    f
}

/// Chart scale: 1, 2, 3, 5 × 10ⁿ at or above `v` (at least `lo`).
fn nice(v: f32, lo: f32) -> f32 {
    for e in -2..10 {
        for m in [1.0f32, 2.0, 3.0, 5.0] {
            let x = m * 10f32.powi(e);
            if x >= v.max(lo) {
                return x;
            }
        }
    }
    1e10
}

/// "12 s", "1 min 40 s", "2 min".
fn age(secs: i64) -> F {
    dur(secs.max(0) as usize)
}

/// FNV-1a hash of a text.
fn hs(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Memo: true when `key` differs from the one stored in `slot`.
fn zm(c: &mut Vec<u64>, slot: usize, key: u64) -> bool {
    if c.len() <= slot {
        c.resize(slot + 1, u64::MAX);
    }
    let ch = c[slot] != key;
    c[slot] = key;
    ch
}

/// "r8169_0_500:00" → "r8169" (per-device suffixes).
fn short_chip(c: &str) -> &str {
    match c.find('_') {
        Some(i) if i > 0 && c[i + 1..].starts_with('0') => &c[..i],
        _ => c,
    }
}

/// Panel heights in a column of height `h`: the minimums in order, then each up to
/// what it wants, the rest to `grow` (or the first one shown).
fn zstack(h: usize, ps: &[(usize, usize)], grow: usize) -> [usize; 5] {
    let mut hs = [0usize; 5];
    let mut left = h;
    for (k, p) in ps.iter().enumerate() {
        if p.0 > 0 && p.0 <= left {
            hs[k] = p.0;
            left -= p.0;
        }
    }
    for (k, p) in ps.iter().enumerate() {
        if hs[k] > 0 {
            let g = p.1.saturating_sub(hs[k]).min(left);
            hs[k] += g;
            left -= g;
        }
    }
    let k = if hs[grow] > 0 {
        grow
    } else {
        hs.iter().position(|&h| h > 0).unwrap_or(0)
    };
    hs[k] += left;
    hs
}

const GORDER: [u8; 7] = [G_CPU, G_BOARD, G_MEM, G_DISK, G_GPU, G_NET, G_OTHER];

impl App {
    // ── layout ─────────────────────────────────────────────────────────────

    /// What the layout depends on.
    fn sens_sig(&self) -> u64 {
        let z = &self.z;
        let mut k = mixk(&[z.rd.len() as u64, z.cpu_power as u64, z.bats.len() as u64]);
        for r in &z.rd {
            k = mixk(&[k, (r.ok as u64) | (r.seen as u64) << 1]);
        }
        k
    }

    /// Lay out again when what's shown changed (a fan started, a sensor appeared).
    pub fn sens_refit(&mut self) -> bool {
        if self.page == PG_SENS && self.zl.sig != self.sens_sig() {
            self.fit(self.w, self.h);
            return true;
        }
        false
    }

    /// Temperature rows (ok, not minor) in group order, and the minor ones.
    fn temp_rows(&self) -> (Vec<usize>, Vec<usize>) {
        let z = &self.z;
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for g in GORDER {
            for (i, r) in z.rd.iter().enumerate() {
                if r.kind == K_TEMP && r.group == g && r.ok {
                    if r.minor { &mut b } else { &mut a }.push(i);
                }
            }
        }
        (a, b)
    }

    /// Fans that ever spun, and the empty headers.
    pub(super) fn fans(&self) -> (Vec<usize>, Vec<usize>) {
        let (mut on, mut off) = (Vec::new(), Vec::new());
        for (i, r) in self.z.rd.iter().enumerate() {
            if r.kind == K_FAN {
                if r.seen { &mut on } else { &mut off }.push(i);
            }
        }
        (on, off)
    }

    /// Voltages: the motherboard's named rails, other chips', unnamed inputs.
    fn volt_items(&self) -> Vec<usize> {
        let z = &self.z;
        let mut v = Vec::new();
        for pass in 0..3 {
            for (i, r) in z.rd.iter().enumerate() {
                let p = if r.guess { 2 } else { (r.group != G_BOARD) as i32 };
                if r.kind == K_VOLT && r.ok && p == pass {
                    v.push(i);
                }
            }
        }
        v
    }

    /// Power readings other than the CPU's own (GPU, RAM, battery...), per-core ones.
    pub(super) fn power_items(&self) -> (Vec<usize>, Vec<usize>) {
        let (mut o, mut c) = (Vec::new(), Vec::new());
        for (i, r) in self.z.rd.iter().enumerate() {
            if r.kind == K_POWER {
                match r.role {
                    R_CORE => c.push(i),
                    R_UNCORE | R_DRAM | R_PSYS | R_GPU | R_OTHER => o.push(i),
                    _ => {}
                }
            }
        }
        (o, c)
    }

    /// Text rows of the power panel.
    fn power_lines(&self) -> usize {
        let z = &self.z;
        let (o, c) = self.power_items();
        let n = match z.cpu_power {
            P_OK => 1 + z.cores_sum().is_some() as usize,
            P_LOCKED => 2,
            _ => 0,
        };
        (n + (!c.is_empty()) as usize + o.len() + z.bats.len()).max(1)
    }

    /// Panels in `r`: three columns (temperatures | fans over power | voltages over
    /// sources), two (temperatures over voltages | fans, power, sources) or one.
    pub fn sens_layout(&self, r: Rect) -> SensLay {
        let (rows, minors) = self.temp_rows();
        let (on, off) = self.fans();
        let nv = self.volt_items().len();
        let z = &self.z;
        let t = (2 + rows.len().clamp(1, 4), 2 + rows.len() + 2 * !minors.is_empty() as usize + 18);
        let f = if on.is_empty() && off.is_empty() && z.fan_hint.is_none() {
            (0, 0)
        } else {
            (3, 4 + on.len().max(1) + !off.is_empty() as usize + if on.is_empty() { 2 } else { 8 })
        };
        let ncores = self.power_items().1.len();
        let p = (3, 10 + self.power_lines() + ncores / 2);
        let s = (3, 5 + 2 * z.ndrivers());
        let v = |w: usize| {
            if nv > 0 {
                (3, 2 + nv.div_ceil((w.saturating_sub(4) / 27).max(1)))
            } else {
                (0, 0)
            }
        };
        // columns: (x, width, panels in order, their slots, growing one)
        let mut out = [Rect::default(); 5];
        let mut col = |x: usize, w: usize, ps: &[(usize, usize)], slots: &[usize], grow: usize| {
            let hs = zstack(r.h, ps, grow);
            let mut y = r.y;
            for (k, &sl) in slots.iter().enumerate() {
                if hs[k] > 0 {
                    out[sl] = Rect { x, y, w, h: hs[k] };
                    y += hs[k];
                }
            }
        };
        // (slots: 0 temperatures, 1 fans, 2 power, 3 voltages, 4 sources)
        if r.w >= 200 && r.h >= 30 {
            let (w1, w2) = (r.w * 36 / 100, r.w * 34 / 100);
            col(r.x, w1, &[(t.0, r.h)], &[0], 0);
            col(r.x + w1, w2, &[f, p], &[1, 2], 1);
            col(r.x + w1 + w2, r.w - w1 - w2, &[v(r.w - w1 - w2), s], &[3, 4], 1);
        } else if r.w >= 100 && r.h >= 24 {
            let w1 = r.w * 54 / 100;
            col(r.x, w1, &[t, v(w1)], &[0, 3], 0);
            col(r.x + w1, r.w - w1, &[f, p, s], &[1, 2, 4], 1);
        } else {
            let fw = if f.0 > 0 { (2 + on.len().clamp(1, 4), f.1.min(4 + on.len())) } else { f };
            let pw = (p.0, 2 + self.power_lines());
            col(r.x, r.w, &[(t.0, 4 + rows.len()), fw, pw, s, v(r.w)], &[0, 1, 2, 4, 3], 0);
        }
        SensLay {
            temp: out[0],
            fan: out[1],
            power: out[2],
            volt: out[3],
            src: out[4],
            sig: self.sens_sig(),
        }
    }

    // ── animation ──────────────────────────────────────────────────────────

    /// New targets after a sample (`smooth`: values glide; else they jump).
    pub fn sens_retarget(&mut self, smooth: bool) {
        if self.page != PG_SENS && self.page != PG_OVERVIEW {
            return;
        }
        let z = &self.z;
        let fresh = self.zv.v.len() != z.rd.len();
        if fresh {
            self.zv.v = vec![Val::default(); z.rd.len()];
        }
        let sm = smooth && !fresh;
        for (v, r) in self.zv.v.iter_mut().zip(&z.rd) {
            v.set(if r.ok { r.val } else { 0.0 }, sm && r.ok && v.to > 0.0);
        }
        self.zv.csum.set(z.cores_sum().unwrap_or(0.0), sm);
    }

    /// Smooth mode: one frame of the springs and digits.
    pub fn sens_step(&mut self, dt: f32) -> bool {
        if self.page != PG_SENS && self.page != PG_OVERVIEW {
            return false;
        }
        let (et, ef, ep) = ((-W_TEMP * dt).exp(), (-W_CLOCK * dt).exp(), (-W_LOAD * dt).exp());
        let mut mv = false;
        for (v, r) in self.zv.v.iter_mut().zip(&self.z.rd) {
            let (w, ex, u) = match r.kind {
                K_TEMP => (W_TEMP, et, 1.0),
                K_FAN => (W_CLOCK, ef, 10.0),
                K_VOLT => (W_CLOCK, ef, 0.01),
                _ => (W_LOAD, ep, wstep(v.x)),
            };
            mv |= v.step(dt, w, ex, u * 0.25);
            mv |= v.digits(dt, u, 0.0);
        }
        let c = &mut self.zv.csum;
        mv |= c.step(dt, W_LOAD, ep, 0.0025);
        mv |= c.digits(dt, wstep(c.x), 0.0);
        mv
    }

    /// Shown value of reading `i` (smooth: gliding).
    pub(super) fn zv(&self, i: usize) -> Val {
        match self.zv.v.get(i) {
            Some(v) => *v,
            None => {
                let mut v = Val::default();
                v.set(self.z.rd[i].val, false);
                v
            }
        }
    }

    /// Title-bar summary: the board, and how many sensor drivers.
    pub fn sens_info(&self) -> (F, F) {
        let z = &self.z;
        let mut a = F::new();
        if !z.board.is_empty() {
            a.str("   ");
            a.str(&z.board);
        }
        let mut b = F::new();
        let n = z.ndrivers();
        if n > 0 {
            b.str(" · ");
            b.int(n as u32, 0);
            b.str(if n > 1 { " sensor drivers" } else { " sensor driver" });
        }
        (a, b)
    }

    // ── drawing helpers ────────────────────────────────────────────────────

    pub fn draw_sens(&mut self, scr: &mut Screen, full: bool, graphs: bool) {
        if full {
            self.zcache.iter_mut().for_each(|c| *c = u64::MAX);
        }
        if self.zv.v.len() != self.z.rd.len() {
            self.sens_retarget(false);
        }
        let l = self.zl;
        if l.temp.w > 0 {
            self.draw_temps(scr, l.temp, full, graphs);
        }
        if l.fan.w > 0 {
            self.draw_fans(scr, l.fan, full, graphs);
        }
        if l.power.w > 0 {
            self.draw_power(scr, l.power, full, graphs);
        }
        if l.volt.w > 0 && (graphs || full) {
            self.draw_volts(scr, l.volt, full);
        }
        if l.src.w > 0 && (graphs || full) {
            self.draw_srcs(scr, l.src, full);
        }
    }

    /// A history series of reading `i` for `n` braille half-columns, `step` seconds
    /// a point, on a grid fixed to the history (columns step left once per `step`),
    /// the live value at the head.
    fn zseries(&self, out: &mut Vec<f32>, n: usize, i: usize, step: usize) {
        let r = &self.z.rd[i];
        out.clear();
        let Some(h) = r.hist.as_ref() else {
            out.resize(n, 0.0);
            return;
        };
        let head = if r.ok { self.zv(i).n } else { 0.0 };
        let off = (self.z.pts % step as u64) as f32;
        Curve {
            head,
            len: h.len(),
            pt: &|k| h.back(k),
            pad: 0.0,
        }
        .sample(out, n, off, step as f32);
        if let Some(l) = out.last_mut() {
            *l = head;
        }
    }

    /// Seconds a point for `n` points to span about 2 minutes or more (eco: at
    /// least 4 s, so graphs move every few samples, not at every one).
    fn zstep(&self, n: usize) -> usize {
        let s = 120usize.div_ceil(n.max(1)).clamp(1, 8);
        if SPEEDS[self.speed].1 == 0 {
            s.max(4)
        } else {
            s
        }
    }

    /// Graph memo key: the history step (eco: every third), and in smooth mode the
    /// live head.
    fn zgk(&self, st: usize, i: usize) -> u64 {
        let eco = SPEEDS[self.speed].1 == 0;
        let head = if self.smooth() {
            self.zv(i).n.to_bits() as u64
        } else {
            0
        };
        mixk(&[self.z.pts / (st * if eco { 3 } else { 1 }) as u64, head, i as u64])
    }

    /// A panel's frame and headline (the first variant that fits); `axis`: a time
    /// axis under a chart (x, width, seconds a point).
    fn ztitle(&self, scr: &mut Screen, r: Rect, full: bool, vs: &[&[(&str, u32)]], axis: Option<(usize, usize, usize)>) {
        let title = Self::pick(r.w, vs);
        if full {
            self.panel(scr, r, title, "");
            if let Some((gx, gw, st)) = axis {
                self.time_axis(scr, r, gx, gw, 2.0 * st as f32, "");
            }
        } else {
            self.panel_top(scr, r, title);
        }
    }

    /// A line chart of reading `a` (and `b` under it, in accent2) at x..x+w, rows
    /// y..y+h, on scale `sc` with the y-axis `labels`; redrawn when its key changes.
    #[allow(clippy::too_many_arguments)]
    fn zchart(&mut self, scr: &mut Screen, slot: usize, full: bool, (x, y, w, h): (usize, usize, usize, usize), a: usize, b: Option<usize>, sc: f32, labels: &[(f32, &str)]) {
        let st = self.zstep(2 * w.saturating_sub(7));
        let k = mixk(&[x as u64, w as u64, h as u64, sc.to_bits() as u64, self.zgk(st, a), b.map_or(1, |b| self.zgk(st, b))]);
        if !(zm(&mut self.zcache, slot, k) || full) {
            return;
        }
        let mut vals = std::mem::take(&mut self.vals);
        self.zseries(&mut vals, 2 * w, a, st);
        let mut v2 = std::mem::take(&mut self.vals2);
        v2.clear();
        if let Some(b) = b {
            self.zseries(&mut v2, 2 * w, b, st);
            self.under.set(self.t.accent2);
        }
        self.vals2 = v2;
        let ramp = crate::color::Ramp([self.t.accent; 65]);
        self.axis_graph(scr, x, y, w, h, &vals, sc, &ramp, labels);
        self.under.set(0);
        self.vals = vals;
    }

    /// Draw list rows `l` in `ir` from `y` to `end`: items as name → value → bar →
    /// note → the hardware's name, in columns sized to fit (the bar gives way first,
    /// then the note, then the raw name); headings and text as they are. A row is
    /// redrawn only when it changed (memo slots `slot` + y), all of them if `all`.
    fn zlist(&mut self, scr: &mut Screen, ir: Rect, l: &ZL, end: usize, all: bool, slot: usize) {
        let (mut nw, mut vw, mut ow, mut rw, mut bars) = (0, 0, 0, 0, false);
        for r in &l.rows {
            if r.kind == 0 {
                let w = |k: usize| l.part(r, k).chars().count();
                (nw, vw, ow, rw) = (nw.max(w(0)), vw.max(w(1)), ow.max(w(2)), rw.max(w(3)));
                bars |= r.frac >= 0.0;
            }
        }
        // (the raw name only if everything fits with a bar of 6 at least)
        let nw = nw.min(26) + 1;
        let rest = ir.w.saturating_sub(nw + vw + 1);
        let bmin = if bars { 4 } else { 0 };
        let raw = rest >= rw + 2 + ow + 1 + bmin;
        let bw = rest.saturating_sub(ow + 1 + if raw { rw + 2 } else { 0 }).min(24);
        let bw = if bars && bw >= 4 { bw } else { 0 };
        let maxx = ir.x + ir.w;
        let th = &self.t;
        let mut y = ir.y;
        for r in &l.rows {
            if y >= end {
                break;
            }
            let k = mixk(&[l.hash(r), fill16(r.frac, bw) as u64, r.vc as u64, (bw + nw) as u64]);
            if zm(&mut self.zcache, slot + y, k) || all {
                let p = |k: usize| l.part(r, k);
                match r.kind {
                    0 => {
                        Self::row(scr, ir.x, y, ir.x + nw + vw + 1, &[(p(0), r.nc, nw as i32), (p(1), r.vc, -(vw as i32)), (" ", 0, 0)]);
                        let x = ir.x + nw + vw + 1;
                        if bw > 0 {
                            let ramp = crate::color::Ramp([r.bc; 65]);
                            if r.frac >= 0.0 {
                                self.meter(scr, x, y, r.frac, bw, &ramp, surf(S_PANEL), LOAD);
                            } else {
                                Self::row(scr, x, y, x + bw, &[]);
                            }
                        }
                        let ow = if raw { ow as i32 + 1 } else { 0 };
                        Self::row(scr, x + bw, y, maxx, &[(" ", 0, 0), (p(2), th.mid, ow), (if raw { p(3) } else { "" }, th.dim, 0)]);
                    }
                    1 => Self::row(scr, ir.x, y, maxx, &[(p(0), th.title | BOLD, 0), (p(2), th.dim, 0)]),
                    _ => Self::row(scr, ir.x, y, maxx, &[(p(0), r.nc, 0), (p(1), r.vc, 0), (p(2), th.dim, 0)]),
                }
            }
            y += 1;
        }
        if all {
            Self::blank(scr, ir.x, &mut y, end, maxx);
        }
    }

    // ── panels ─────────────────────────────────────────────────────────────

    /// Temperatures: rows by part (a heading per part when there's room), the minor
    /// ones in a line per part, and small over-time graphs of the key ones.
    fn draw_temps(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let (rows, minors) = self.temp_rows();
        let z = &self.z;
        // the worst identified reading (unlabelled inputs don't count), the parts
        let (mut worst, mut hot) = (0u8, usize::MAX);
        let (mut keys, mut groups, mut mg) = (Vec::new(), Vec::new(), Vec::new());
        for &i in &rows {
            let rr = &z.rd[i];
            let lv = level(self.zv(i).n, rr.warn, rr.crit);
            if !rr.guess && (hot == usize::MAX || lv > worst) {
                (worst, hot) = (lv, i);
            }
            if !groups.contains(&rr.group) {
                groups.push(rr.group);
                if rr.hist.is_some() && !rr.guess {
                    keys.push(i);
                }
            }
        }
        for &i in &minors {
            if !mg.contains(&z.rd[i].group) {
                mg.push(z.rd[i].group);
            }
        }
        let free = ih.saturating_sub(rows.len() + mg.len());
        let heads = iw >= 30 && free >= groups.len() + if keys.is_empty() { 0 } else { 6 };
        let free = free - if heads { groups.len() } else { 0 };
        let (nch, chh) = if free >= 6 && iw >= 34 && !keys.is_empty() {
            let n = keys.len().min((free - 2) / 3).max(1);
            (n, ((free - 2) / n).min(6))
        } else {
            (0, 0)
        };
        let y_chart = ir.y + ih - nch * chh;
        let (lg, gw) = (14, iw.saturating_sub(14));
        let st = self.zstep(2 * gw);
        let th = &self.t;
        let mut t = F::new();
        if hot < z.rd.len() {
            t.str(if worst > 0 { " · " } else { " · hottest: " });
            t.str(&z.rd[hot].name);
            t.push(b' ');
            t.str(deg(self.zv(hot).n).s());
        }
        if zm(&mut self.zcache, Z_TEMP, mixk(&[hs(t.s()), worst as u64, iw as u64])) || full {
            let lab = ("temperatures", th.title | BOLD);
            let v = if rows.is_empty() {
                (" none found", th.dim)
            } else {
                [(" all normal", th.good), (" warm", th.warn), (" too hot", th.crit | BOLD)][worst.min(2) as usize]
            };
            self.ztitle(scr, r, full, &[&[lab, v, (t.s(), th.text)], &[lab, v]], (nch > 0).then_some((ir.x + lg, gw, st)));
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let mut l = std::mem::take(&mut self.zv.l);
        l.clear();
        let th = &self.t;
        if rows.is_empty() {
            l.row(2, th.dim, 0, -1.0, 0);
            l.s(if z.vm { "no temperature sensors (a virtual machine: the host has them)" } else { "no temperature sensors found" });
        }
        let mut last = 255;
        for &i in &rows {
            let rr = &z.rd[i];
            if heads && rr.group != last {
                last = rr.group;
                l.row(1, 0, 0, -1.0, 0);
                l.s(group_name(last)).f().f().s(" · ").s(short_chip(&z.srcs[rr.src as usize].chip));
                if rows.iter().any(|&j| z.rd[j].group == last && z.rd[j].guess) {
                    l.s(" · aux = unlabelled inputs, maybe unused");
                }
            }
            let v = self.zv(i);
            // (unlabelled inputs aren't judged: their colour stays neutral)
            let c = if rr.guess { th.mid } else { self.hcol(level(v.n, rr.warn, rr.crit)) };
            l.row(0, if rr.guess { th.mid } else { th.text }, c | BOLD, v.x / rr.crit.max(1.0), c);
            l.s(if heads { "  " } else { "" }).s(&rr.name).f().s(deg(v.n).s()).f().s(num("of ", rr.crit, 0, "°").s());
            if rr.hist.is_some() {
                l.s(num(" · peak ", rr.peak.max(v.n), 0, "°").s());
            }
            l.f().s(&rr.raw);
        }
        // the minor readings, one line per part: "CPU also: core 0 45°C · core 1 47°C"
        for g in mg {
            l.row(2, th.dim, th.mid, -1.0, 0);
            l.s(group_name(g)).s(" also:").f();
            let mut sep = " ";
            for &i in &minors {
                if z.rd[i].group == g {
                    l.s(sep).s(&z.rd[i].name).s(" ").s(deg(self.zv(i).n).s());
                    sep = " · ";
                }
            }
        }
        let all = zm(&mut self.zcache, Z_TLAY, mixk(&[iw as u64, ih as u64, nch as u64, l.rows.len() as u64])) || full;
        let end = y_chart.saturating_sub((nch > 0) as usize).max(ir.y);
        self.zlist(scr, ir, &l, end, all, 200);
        self.zv.l = l;
        if nch > 0 && all {
            let th = &self.t;
            Self::row(scr, ir.x, end, ir.x + iw, &[("temperatures over time", th.mid, 0), (" · each graph 20–100 °C", th.dim, 0)]);
        }
        if nch == 0 || !(graphs || full) {
            return;
        }
        // the graphs: name and value at the left, the history to the right
        let mut ck = mixk(&[iw as u64, ih as u64, nch as u64, chh as u64]);
        for &i in keys.iter().take(nch) {
            ck = mixk(&[ck, self.zgk(st, i)]);
        }
        if !(zm(&mut self.zcache, Z_TGRAPH, ck) || full) {
            return;
        }
        let mut vals = std::mem::take(&mut self.vals);
        for (k, &i) in keys.iter().take(nch).enumerate() {
            let y0 = y_chart + k * chh;
            self.zseries(&mut vals, 2 * gw, i, st);
            for v in vals.iter_mut() {
                *v = if *v > 0.0 { (*v - 20.0).max(0.5) } else { 0.0 };
            }
            let th = &self.t;
            let rr = &self.z.rd[i];
            let v = self.zv(i).n;
            let c = self.hcol(level(v, rr.warn, rr.crit));
            let dv = deg(v);
            for yy in 0..chh {
                let sp = [(rr.name.as_str(), th.text), (dv.s(), c | BOLD), ("", 0)][yy.min(2)];
                Self::line(scr, ir.x, y0 + yy, ir.x + lg, surf(S_PANEL), &[sp]);
            }
            let ramp = crate::color::Ramp([c; 65]);
            self.graph(scr, ir.x + lg, y0, gw, chh, &vals, 80.0, &ramp);
        }
        self.vals = vals;
    }

    /// Fans: each spinning fan with its speed, how hard the board drives it and its
    /// header; the empty headers in one line; speed over time.
    fn draw_fans(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let (on, off) = self.fans();
        let eco = SPEEDS[self.speed].1 == 0;
        let z = &self.z;
        // a CPU fan that spun and now reads 0
        let stopped = on.iter().copied().find(|&i| {
            let r = &z.rd[i];
            r.name.starts_with("CPU fan") && r.ok && r.val == 0.0 && r.max > 300.0
        });
        let src = on.first().or(off.first()).map_or(0, |&i| z.rd[i].src as usize);
        let slow = if z.srcs.is_empty() { 0 } else { z.period(src, eco) / SEC };
        let text = on.len().max(1) + !off.is_empty() as usize + (slow >= 10) as usize + 2 * on.is_empty() as usize;
        let chart = !on.is_empty() && ih >= text + 6 && iw >= 30;
        let gh = if chart { ih - text - 2 } else { 0 };
        let gw = iw.saturating_sub(7);
        let st = self.zstep(2 * gw);
        let th = &self.t;
        let mut t = F::new();
        let mut tc = th.text;
        if let Some(i) = stopped.or(on.first().copied()) {
            t.str(" · ");
            t.str(&z.rd[i].name);
            if stopped.is_some() {
                t.str(" stopped!");
                tc = th.crit | BOLD;
            } else {
                t.str(num(" ", self.zv(i).n, 0, " RPM").s());
            }
        }
        if zm(&mut self.zcache, Z_FAN, mixk(&[hs(t.s()), on.len() as u64, iw as u64])) || full {
            let lab = ("fans", th.title | BOLD);
            let n = num(" ", on.len() as f32, 0, " spinning");
            let v = if on.is_empty() { (" none found", th.warn) } else { (n.s(), th.good) };
            self.ztitle(scr, r, full, &[&[lab, v, (t.s(), tc)], &[lab, v]], chart.then_some((ir.x + 7, gw, st)));
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let mut l = std::mem::take(&mut self.zv.l);
        l.clear();
        let th = &self.t;
        for &i in &on {
            let rr = &z.rd[i];
            let v = self.zv(i);
            let duty = rr.pwm;
            // the bar: against the fastest seen, or what the duty suggests at 100 %
            let top = rr.max.max(if duty > 0.1 { v.x / duty } else { 0.0 }).max(v.x).max(1.0);
            l.row(0, th.text, th.fg | BOLD, v.x / top, if v.n > 0.0 { th.accent } else { th.dim });
            l.s(&rr.name).f().s(num("", v.n, 0, " RPM").s()).f();
            if v.n == 0.0 && rr.ok {
                l.s("stopped");
            } else if duty >= 0.0 {
                l.s(match rr.en {
                    0 => "full speed",
                    1 => "fixed at ",
                    _ => "board sets ",
                });
                if rr.en != 0 {
                    l.s(num("", duty * 100.0, 0, "%").s());
                }
            }
            l.f().s(&rr.raw);
        }
        if on.is_empty() {
            self.fan_hint_text(&mut l, true);
        }
        if !off.is_empty() {
            l.row(2, th.mid, th.dim, -1.0, 0);
            l.s(num("", off.len() as f32, 0, if off.len() == 1 { " header with nothing connected" } else { " headers with nothing connected" }).s());
            if iw >= 34 + 5 * off.len() {
                l.f().s(":");
                for &i in &off {
                    let raw = &z.rd[i].raw;
                    l.s(" ").s(&raw[raw.rfind(' ').map_or(0, |k| k + 1)..]);
                }
            }
        }
        if slow >= 10 {
            // the board chip is slow to read: say how fresh the numbers are
            let a = z.age(src, crate::term::mono()) / 15 * 15;
            l.row(2, th.mid, th.mid, -1.0, 0);
            l.s(if a == 0 { "updated just now" } else { "updated " });
            if a > 0 {
                l.s(age(a).s()).s(" ago");
            }
            if iw >= 50 {
                l.f().f().s(" · read every ").s(age(slow).s());
                if eco && iw >= 90 {
                    l.s(" in eco (each read costs ").s(dec(z.srcs[src].cost as f32 / 1e6, 1).s()).s(" ms of CPU; s: faster)");
                }
            }
        }
        let end = ir.y + ih - if chart { gh + 1 } else { 0 };
        let all = zm(&mut self.zcache, Z_FBODY, mixk(&[iw as u64, ih as u64, l.rows.len() as u64])) || full;
        self.zlist(scr, ir, &l, end, all, 300);
        self.zv.l = l;
        if !chart {
            return;
        }
        let z = &self.z;
        let (a, b) = (on[0], on.get(1).copied());
        if all {
            let th = &self.t;
            let sec = b.map_or("", |i| z.rd[i].name.as_str());
            let leg = [("fan speed over time (RPM)", th.mid, 0), ("  ■ ", th.accent, 0), (&z.rd[a].name, th.text, 0), ("  ■ ", th.accent2, 0), (sec, th.text, 0)];
            Self::row(scr, ir.x, end, ir.x + iw, &leg[..if sec.is_empty() || iw < 60 { 1 } else { 5 }]);
        }
        if graphs || full {
            let mut top = 0.0f32;
            for &i in &on {
                top = top.max(z.rd[i].max).max(z.rd[i].peak);
            }
            let sc = nice(top * 1.05, 500.0);
            let (l1, l2) = (num("", sc, 0, ""), num("", sc / 2.0, 0, ""));
            self.zchart(scr, Z_FGRAPH, full, (ir.x, end + 1, iw, gh), a, b, sc, &[(1.0, l1.s()), (0.5, l2.s()), (0.0, "0")]);
        }
    }

    /// Why there are no fans, and what to do: a warning line and a dim one (`two`)
    /// or both on one line.
    fn fan_hint_text(&self, l: &mut ZL, two: bool) {
        let z = &self.z;
        let th = &self.t;
        let m = z.fan_hint.as_ref().map_or("", |h| h.0.as_str());
        l.row(2, th.warn, th.dim, -1.0, 0);
        let b = match z.fan_hint.as_ref().map(|h| h.1) {
            _ if z.vm => {
                l.s("no fans: this is a virtual machine");
                "fans, temperatures and power belong to the host"
            }
            Some(0) => {
                l.s("no fan sensors: the ").s(m).s(" driver isn't loaded");
                "run `cpuview --setup-sensors` to load it (asks first)"
            }
            Some(3) if z.bmc_locked => {
                l.s("no fan sensors: the board's BMC runs the fans (IPMI), root-only");
                "run `cpuview --setup-sensors --bmc` to read them (asks first)"
            }
            Some(4) => {
                l.s("no fans, CPU temperature or CPU power: Windows has no interface for them");
                "run LibreHardwareMonitor with its web server on (cpuview --setup-sensors says how)"
            }
            Some(3) => {
                l.s("no fan sensors: the board's BMC runs the fans (IPMI) and lists none");
                "`sudo ipmitool sdr type Fan` shows what it has"
            }
            Some(1) => {
                l.s("no fan sensors, although ").s(m).s(" is loaded");
                "`sudo sensors-detect` (lm-sensors) may find the right driver"
            }
            _ => {
                l.s("no fan sensors found");
                "`sudo sensors-detect` (lm-sensors) can look for a driver"
            }
        };
        if two {
            l.row(2, th.dim, 0, -1.0, 0);
        } else {
            l.f().s(" — ");
        }
        l.s(b);
    }

    /// Power: CPU package (cores vs the rest, each core where the CPU counts it),
    /// GPU, battery; power over time.
    fn draw_power(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let z = &self.z;
        let pkg = z.role(R_PKG);
        let gpu = z.role(R_GPU).or(z.role(R_UNCORE)).filter(|&i| z.rd[i].ok);
        let (others, cores) = self.power_items();
        // the chart follows the CPU, else the whole computer, the battery, the GPU
        let chart_i = [R_PKG, R_PSYS, R_BAT, R_GPU].iter().filter_map(|&ro| z.role(ro)).find(|&i| z.rd[i].hist.is_some());
        let per = (iw.saturating_sub(11) / 14).max(1);
        let nl = self.power_lines() + cores.len().div_ceil(per);
        let chart = chart_i.is_some() && ih >= nl + 6 && iw >= 30;
        let gh = if chart { ih - nl - 2 } else { 0 };
        let gw = iw.saturating_sub(7);
        let st = self.zstep(2 * gw);
        let th = &self.t;
        let mut t = F::new();
        let mut tc = th.accent | BOLD;
        match (z.cpu_power, pkg) {
            (P_OK, Some(i)) if z.rd[i].ok => t.str(fc(&[" CPU ", watts(self.zv(i).n).s()]).s()),
            (P_OK, _) => t.str(" CPU measuring…"),
            (P_LOCKED, _) => {
                t.str(" CPU needs permission");
                tc = th.warn;
            }
            _ => {}
        }
        let mut g = F::new();
        if let Some(i) = gpu {
            g.str(" · GPU ");
            g.str(watts(self.zv(i).n).s());
        }
        if zm(&mut self.zcache, Z_POW, mixk(&[hs(t.s()), hs(g.s()), iw as u64])) || full {
            let lab = ("power", th.title | BOLD);
            self.ztitle(scr, r, full, &[&[lab, (t.s(), tc), (g.s(), th.accent2 | BOLD)], &[lab, (t.s(), tc)]], chart.then_some((ir.x + 7, gw, st)));
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let mut l = std::mem::take(&mut self.zv.l);
        l.clear();
        let th = &self.t;
        // a reading: name, watts (or "measuring"), the hardware's name
        let item = |l: &mut ZL, name: &str, i: usize, c: u32| {
            let rr = &z.rd[i];
            l.row(0, th.text, c | BOLD, -1.0, 0);
            l.s(name).f().s(if rr.ok { watts(self.zv(i).n) } else { fc(&["…"]) }.s()).f().s(if rr.ok { "" } else { "measuring" });
            l.f().s(short_chip(&z.srcs[rr.src as usize].chip)).s(" ").s(&rr.raw);
        };
        if z.cpu_power == P_LOCKED {
            l.row(2, th.warn, th.fg | BOLD, -1.0, 0);
            l.s("CPU power needs permission: run ").f().s("cpuview --setup-sensors");
            l.row(2, th.dim, 0, -1.0, 0);
            l.s("(energy counters root-only");
            if z.perf_paranoid > 2 {
                l.s(num("; perf_event_paranoid ", z.perf_paranoid as f32, 0, "").s());
            }
            l.s(")");
        }
        if let (Some(i), P_OK) = (pkg, z.cpu_power) {
            item(&mut l, "CPU package", i, th.accent);
            if let (Some(_), true) = (z.cores_sum(), z.rd[i].ok) {
                let (c, v) = (self.zv.csum.n, self.zv(i).n);
                l.row(2, th.text, th.fg | BOLD, -1.0, 0);
                l.s("  cores ").f().s(watts(c).s()).f().s(num(" (", c / v.max(0.01) * 100.0, 0, "%) · rest of the CPU (cache, memory & I/O) ").s()).s(watts((v - c).max(0.0)).s());
            }
        }
        // each core, a few to a line
        for (k, ch) in cores.chunks(per).enumerate() {
            l.row(2, th.mid, th.text, -1.0, 0);
            l.s(if k == 0 { "  per core:" } else { "           " }).f();
            for &i in ch {
                l.s("  ").s(&z.rd[i].name).s(" ").s(dec(self.zv(i).n, 1).s()).s(" W");
            }
        }
        for &i in &others {
            item(&mut l, &z.rd[i].name, i, th.accent2);
        }
        for b in &z.bats {
            // battery: watts (or the charge when there's no flow), then the state
            let c = if b.st == 2 && b.pct < 20.0 { self.hcol(1 + (b.pct < 10.0) as u8) } else { th.fg };
            l.row(0, th.text, c | BOLD, -1.0, 0);
            l.s("battery").f().s(if b.w > 0.05 { watts(b.w) } else { num("", b.pct, 0, "%") }.s()).f();
            l.s(num("", b.pct, 0, "%").s());
            if b.left <= 0.0 || b.st != 2 {
                l.s(" · ").s(match b.st {
                    1 => "charging",
                    2 => "on battery",
                    3 => "full",
                    4 => "plugged in, not charging",
                    _ => "",
                });
            }
            if b.left > 0.0 {
                let m = (b.left / 60.0) as u32;
                l.s(" · ");
                if m >= 60 {
                    l.s(num("", (m / 60) as f32, 0, " h ").s());
                }
                l.s(num("", (m % 60) as f32, 0, if b.st == 1 { " min to full" } else { " min left" }).s());
            }
            l.f().s(&b.name);
        }
        if z.cpu_power == P_NONE && others.is_empty() && z.bats.is_empty() {
            l.row(2, th.dim, 0, -1.0, 0);
            l.s(if z.vm {
                "no power meters (a virtual machine)"
            } else if cfg!(windows) {
                "no power meters: Windows has none (LibreHardwareMonitor can)"
            } else {
                "no power meters on this machine"
            });
        }
        let end = ir.y + ih - if chart { gh + 1 } else { 0 };
        let all = zm(&mut self.zcache, Z_PBODY, mixk(&[iw as u64, ih as u64, l.rows.len() as u64])) || full;
        self.zlist(scr, ir, &l, end, all, 400);
        self.zv.l = l;
        let Some(i) = chart_i.filter(|_| chart) else { return };
        if all {
            let what = ["CPU", "computer", "battery", "GPU"][match self.z.rd[i].role {
                R_PKG => 0,
                R_PSYS => 1,
                R_BAT => 2,
                _ => 3,
            }];
            Self::row(scr, ir.x, end, ir.x + iw, &[(what, self.t.mid, 0), (" power over time (watts)", self.t.mid, 0)]);
        }
        if graphs || full {
            let mut top = self.zv(i).n;
            if let Some(h) = self.z.rd[i].hist.as_ref() {
                for k in 0..h.len().min(gw * 2 * st) {
                    top = top.max(h.back(k));
                }
            }
            let sc = nice(top * 1.05, 0.1);
            let (l1, l2) = (watts(sc), watts(sc / 2.0));
            self.zchart(scr, Z_PGRAPH, full, (ir.x, end + 1, iw, gh), i, None, sc, &[(1.0, l1.s()), (0.5, l2.s()), (0.0, "0 W")]);
        }
    }

    /// Name (left, `nw` wide) and value (right-aligned, `vw` wide); returns the x
    /// after them.
    #[allow(clippy::too_many_arguments)]
    fn zrow(scr: &mut Screen, x: usize, y: usize, name: &str, nc: u32, nw: usize, val: &str, vc: u32, vw: usize) -> usize {
        Self::row(scr, x, y, x + nw + vw + 1, &[(name, nc, nw as i32), (val, vc, -(vw as i32)), (" ", 0, 0)]);
        x + nw + vw + 1
    }

    /// Voltages, compactly in columns; only rails outside their chip limits stand out.
    fn draw_volts(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let items = self.volt_items();
        let z = &self.z;
        let out = |i: usize| {
            let r = &z.rd[i];
            (r.crit > 0.0 && r.val > r.crit * 1.02) || (r.warn > 0.0 && r.val < r.warn * 0.98)
        };
        let mut nbad = 0;
        let mut lims = false;
        let mut k = mixk(&[iw as u64, ih as u64]);
        for &i in &items {
            nbad += out(i) as usize;
            lims |= z.rd[i].crit > 0.0;
            k = mixk(&[k, (self.zv(i).n * 100.0).round() as u64]);
        }
        if !(zm(&mut self.zcache, Z_VOLT, k) || full) {
            return;
        }
        let th = &self.t;
        let lab = ("voltages", th.title | BOLD);
        let b = num(" · ", nbad as f32, 0, " outside their limits");
        let t = if nbad > 0 {
            (b.s(), th.crit | BOLD)
        } else if lims {
            (" · within limits", th.good)
        } else {
            ("", 0)
        };
        self.panel(scr, r, &[lab, t], "");
        if ih == 0 || iw < 12 {
            return;
        }
        let raw = iw / 33 >= 2;
        let cw = if raw { 33 } else { 27 };
        let cols = (iw / cw).max(1);
        let need = items.len().div_ceil(cols);
        let rows = need.min(ih);
        // (what doesn't fit: the last ones, unnamed inputs first)
        let shown = if need > ih { (rows * cols).saturating_sub(1) } else { items.len() };
        let maxx = ir.x + iw;
        for row in 0..rows {
            let y = ir.y + row;
            Self::row(scr, ir.x, y, maxx, &[]);
            for c in 0..cols {
                let k = c * rows + row;
                let Some(&i) = items.get(k).filter(|_| k < shown) else { continue };
                let rr = &z.rd[i];
                let x = ir.x + c * cw;
                let x2 = Self::zrow(scr, x, y, &rr.name, if rr.guess { th.dim } else { th.text }, 18, volts(self.zv(i).n).s(), if out(i) { th.crit | BOLD } else { th.fg }, 7);
                if raw && !rr.guess {
                    Self::row(scr, x2, y, (x + cw).min(maxx), &[(&rr.raw, th.dim, 0)]);
                }
            }
        }
        if shown < items.len() {
            let more = num("+ ", (items.len() - shown) as f32, 0, " more");
            Self::row(scr, ir.x + (cols - 1) * cw, ir.y + rows - 1, maxx, &[(more.s(), th.dim, 0)]);
        }
    }

    /// Where the readings come from: each kind of chip with its driver, what it is
    /// and shows (slow ones: how often they're read), the board, how CPU power is
    /// read (or why not), and what to load for missing fans.
    fn draw_srcs(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let eco = SPEEDS[self.speed].1 == 0;
        let z = &self.z;
        let mut k = mixk(&[ir.w as u64, ir.h as u64, z.srcs.len() as u64, z.cpu_power as u64]);
        for i in 0..z.srcs.len() {
            k = mixk(&[k, (z.period(i, eco) / 10 / SEC) as u64]);
        }
        if !(zm(&mut self.zcache, Z_SRC, k) || full) {
            return;
        }
        let th = &self.t;
        let n = num(" ", z.ndrivers() as f32, 0, " drivers");
        self.panel(scr, r, &[("sensor sources", th.title | BOLD), (n.s(), th.mid)], "");
        if ir.h == 0 || ir.w < 12 {
            return;
        }
        let mut l = std::mem::take(&mut self.zv.l);
        l.clear();
        if !z.board.is_empty() {
            l.row(2, th.dim, th.text, -1.0, 0);
            l.s("board ").f().s(&z.board);
        }
        // one row per kind of chip (two DIMM sensors: "spd5118 ×2")
        for (i, s) in z.srcs.iter().enumerate() {
            let name = short_chip(&s.chip);
            if z.srcs[..i].iter().any(|o| short_chip(&o.chip) == name) {
                continue;
            }
            let mut n = [0u16; 4];
            let mut cnt = 0;
            for o in &z.srcs[i..] {
                if short_chip(&o.chip) == name {
                    cnt += 1;
                    for (a, b) in n.iter_mut().zip(o.n) {
                        *a += b;
                    }
                }
            }
            l.row(0, th.fg | BOLD, 0, -1.0, 0);
            l.s(name);
            if cnt > 1 {
                l.s(num(" ×", cnt as f32, 0, "").s());
            }
            l.f().f();
            let wide = ir.w >= 70;
            let mut head = 16 + s.what.len();
            if wide && s.driver != name && !s.driver.is_empty() && !s.driver.contains(' ') {
                l.s("(").s(&s.driver).s(") ");
                head += s.driver.len() + 3;
            }
            l.s(&s.what);
            let mut d = F::new();
            for (k, c) in n.iter().enumerate() {
                if *c > 0 {
                    d.str(if d.n > 0 { ", " } else { "" });
                    let w = match k {
                        0 => " fan",
                        1 => " temp",
                        2 => " voltage",
                        _ => " power meter",
                    };
                    d.str(num("", *c as f32, 0, w).s());
                    d.str(if *c > 1 { "s" } else { "" });
                }
            }
            d.str(if d.n == 0 { "no readings" } else { "" });
            if matches!(s.group, G_NET | G_DISK) {
                for o in &z.srcs[i..] {
                    if short_chip(&o.chip) == name {
                        d.str(" · ");
                        d.str(if wide { &o.dev } else { o.dev.split(' ').next().unwrap_or("") });
                    }
                }
            }
            let p = z.period(i, eco) / SEC;
            let mut slow = F::new();
            if p >= 10 && s.cost > 200_000 {
                slow.str(" · read every ");
                slow.str(age(p).s());
                if wide {
                    slow.str(" (");
                    slow.str(dec(s.cost as f32 / 1e6, 1).s());
                    slow.str(" ms of CPU each)");
                }
            }
            // (what doesn't fit on a line of its own)
            let (dn, sn) = (d.s().chars().count(), slow.s().chars().count());
            if head + dn + sn > ir.w {
                if head + dn > ir.w {
                    l.row(2, th.mid, 0, -1.0, 0);
                    l.s("    ");
                } else {
                    l.s(" · ");
                }
                l.s(d.s());
                if sn > 0 {
                    l.row(2, th.mid, 0, -1.0, 0);
                    l.s("    ").s(&slow.s()[4..]);
                }
            } else {
                l.s(" · ").s(d.s()).s(slow.s());
            }
        }
        l.row(2, th.mid, th.dim, -1.0, 0);
        match z.cpu_power {
            P_OK => {
                l.s(if cfg!(windows) { "CPU power: LibreHardwareMonitor" } else { "CPU power: RAPL energy counters" });
                if !z.rd.iter().any(|r| r.role == R_CORE) && z.has_core_pmu() {
                    l.f().s(" · per core: cpuview --setup-sensors --per-core");
                }
            }
            P_LOCKED => {
                l.rows.last_mut().unwrap().nc = th.warn;
                l.s("CPU power: locked");
                l.s(if z.rapl_locked { " · RAPL is root-only" } else { "" });
                l.s(if z.perf_locked { " · perf denied" } else { "" });
                l.row(2, th.dim, th.fg, -1.0, 0);
                l.s("  to allow it: ").f().s("cpuview --setup-sensors");
            }
            _ => {
                l.s(if cfg!(windows) {
                    "CPU power: not readable on Windows without LibreHardwareMonitor"
                } else {
                    "CPU power: no energy counters on this CPU"
                });
            }
        }
        if z.fan_hint.is_some() || (z.vm && !z.rd.iter().any(|r| r.kind == K_FAN)) {
            self.fan_hint_text(&mut l, false);
        }
        self.zlist(scr, ir, &l, ir.y + ir.h, true, 500);
        self.zv.l = l;
    }
}

/// List rows for `App::zlist`, their texts in one buffer (no allocation per frame),
/// written field by field: `row`, then `s` (text) and `f` (next field).
#[derive(Default)]
pub struct ZL {
    buf: String,
    rows: Vec<ZR>,
    field: usize,
}

/// A row: kind (0 item, 1 heading, 2 text), where its parts end in `buf` (name,
/// value, note, raw name), colours of name, value and bar, the bar's fill (< 0: none).
#[derive(Clone, Copy)]
struct ZR {
    kind: u8,
    at: [u16; 5],
    nc: u32,
    vc: u32,
    bc: u32,
    frac: f32,
}

impl ZL {
    fn clear(&mut self) {
        self.buf.clear();
        self.rows.clear();
    }

    /// A new row; its name follows.
    fn row(&mut self, kind: u8, nc: u32, vc: u32, frac: f32, bc: u32) {
        let n = self.buf.len() as u16;
        self.rows.push(ZR { kind, at: [n; 5], nc, vc, bc, frac });
        self.field = 0;
    }

    /// Text into the current field.
    fn s(&mut self, s: &str) -> &mut ZL {
        self.buf.push_str(s);
        let n = self.buf.len() as u16;
        if let Some(r) = self.rows.last_mut() {
            for a in &mut r.at[self.field + 1..] {
                *a = n;
            }
        }
        self
    }

    /// On to the next field (name → value → note → raw name).
    fn f(&mut self) -> &mut ZL {
        self.field = (self.field + 1).min(3);
        self
    }

    /// Part `k` (0 name, 1 value, 2 note, 3 raw name) of row `r`.
    fn part(&self, r: &ZR, k: usize) -> &str {
        self.buf.get(r.at[k] as usize..r.at[k + 1] as usize).unwrap_or("")
    }

    fn hash(&self, r: &ZR) -> u64 {
        hs(self.buf.get(r.at[0] as usize..r.at[4] as usize).unwrap_or(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formats() {
        assert_eq!(watts(117.4).s(), "117 W");
        assert_eq!(watts(43.21).s(), "43.2 W");
        assert_eq!(watts(5.624).s(), "5.62 W");
        assert_eq!(watts(0.008).s(), "0.01 W");
        assert_eq!(watts(0.003).s(), "<0.01 W");
        assert_eq!(deg(49.5).s(), "50°C");
        assert_eq!(deg(-3.0).s(), "-3°C");
        assert_eq!(volts(1.032).s(), "1.03 V");
        assert_eq!(volts(12.096).s(), "12.10 V");
        assert_eq!(nice(1459.0, 500.0), 2000.0);
        assert_eq!(nice(117.0, 0.1), 200.0);
        assert_eq!(nice(0.02, 0.1), 0.1);
        assert_eq!(zstack(20, &[(3, 8), (3, 30)], 1)[..2], [8, 12]);
        assert_eq!(short_chip("r8169_0_500:00"), "r8169");
    }
}

//! Network & System page (page 3, key `3`): per-link download / upload speeds with a
//! rate-over-time chart, connections and listening ports, containers and services.
//! Panels for things this machine doesn't have (containers, systemd) don't appear.
//!
//! "Right now" values are horizontal bars; "over time" charts are vertical columns
//! with a time axis ("2m ago … now"), a y axis in round units and a legend.

use super::*;
use crate::anim::{Val, W_LOAD};
use crate::net::*;

/// Network-page panels (`w == 0`: not shown); `soon`: the page-4 placeholder.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct NetLay {
    pub net: Rect,
    pub conn: Rect,
    pub ctr: Rect,
    pub svc: Rect,
    pub soon: Rect,
    /// what the layout was made for (containers, services, links up / down, virtual)
    sig: u64,
}

impl NetLay {
    pub fn rects(&self) -> [Rect; 5] {
        [self.net, self.conn, self.ctr, self.svc, self.soon]
    }
}

/// An animated list row: values (`v[0..4]`) and its position (`v[4]`), which glides
/// when the order changes.
#[derive(Clone, Copy, Default)]
pub struct Item {
    key: u64,
    idx: usize,
    v: [Val; 5],
}

#[derive(Default)]
pub struct List {
    it: Vec<Item>,
}

impl List {
    /// New data (key, values, sort key), sorted biggest first. The order shown changes
    /// only when an item clearly overtakes the one above it (by `hy` × + `abs`).
    fn set(&mut self, new: &[(u64, [f32; 4], f32)], smooth: bool, hy: f32, abs: f32) {
        self.it.retain(|i| new.iter().any(|n| n.0 == i.key));
        for (j, (k, v, _)) in new.iter().enumerate() {
            if let Some(i) = self.it.iter_mut().find(|i| i.key == *k) {
                for (x, &t) in i.v.iter_mut().zip(v) {
                    x.set(t, smooth);
                }
                i.idx = j;
            } else {
                let mut i = Item {
                    key: *k,
                    idx: j,
                    ..Default::default()
                };
                for (x, &t) in i.v.iter_mut().zip(v) {
                    x.set(t, false);
                }
                i.v[4].set(self.it.len() as f32, false);
                self.it.push(i);
            }
        }
        let n = self.it.len();
        for _ in 0..n {
            let mut sw = false;
            for a in 1..n {
                if new[self.it[a].idx].2 > new[self.it[a - 1].idx].2 * hy + abs {
                    self.it.swap(a - 1, a);
                    sw = true;
                }
            }
            if !sw {
                break;
            }
        }
        for (k, i) in self.it.iter_mut().enumerate() {
            i.v[4].set(k as f32, smooth);
        }
    }

    /// Smooth mode: one frame. `unit(q, x)`: the display resolution of value q.
    fn step(&mut self, dt: f32, ex: f32, unit: &dyn Fn(usize, f32) -> f32) -> bool {
        let mut mv = false;
        for i in &mut self.it {
            for q in 0..4 {
                let u = unit(q, i.v[q].x);
                mv |= i.v[q].step(dt, W_LOAD, ex, u * 0.25);
                mv |= i.v[q].digits(dt, u, 0.0);
            }
            mv |= i.v[4].step(dt, W_LOAD, ex, 0.01);
        }
        mv
    }

    /// Rows in display order (by gliding position).
    fn order(&self) -> impl Iterator<Item = &Item> {
        let mut o: Vec<usize> = (0..self.it.len()).collect();
        crate::sys::isort(&mut o, &|&a, &b| self.it[a].v[4].x < self.it[b].v[4].x);
        o.into_iter().map(|i| &self.it[i])
    }

    fn key(&self, bars: usize, top: f32) -> u64 {
        let mut k = self.it.len() as u64;
        for i in &self.it {
            k = mixk(&[
                k,
                i.key,
                i.v[0].n.to_bits() as u64,
                i.v[1].n.to_bits() as u64,
                i.v[2].n.to_bits() as u64,
                i.v[3].n.to_bits() as u64,
                (i.v[4].x * 4.0) as u64,
                fill16(i.v[0].x / top, bars) as u64,
            ]);
        }
        k
    }
}

/// Animated values of the network page.
#[derive(Default)]
pub struct NA {
    /// per link (by name): download, upload, and their chart scales
    links: List,
    estab: Val,
    tw: Val,
    ctrs: List,
    svc_cpu: List,
    svc_mem: List,
    /// everything the logged-in users run (user.slice), one row
    users: List,
    peers: List,
}

const N_TEXT: usize = 0;
const N_CHART: usize = 1;
const N_CONN: usize = 2;
const N_CTR: usize = 3;
const N_SVC: usize = 4;
const N_AXIS: usize = 5;

pub const RATE: [&str; 6] = ["B/s", "kB/s", "MB/s", "GB/s", "TB/s", "PB/s"];
pub const SIZE: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];

fn dunit(v: f32) -> (f32, usize) {
    let (mut v, mut k) = (v.max(0.0), 0);
    while v >= 999.5 && k < 5 {
        v /= 1000.0;
        k += 1;
    }
    (v, k)
}

/// Short decimal: "73.5k", "512", "1.20M".
pub fn dshort(v: f32) -> F {
    dfmt_sp(v, &SIZE, false)
}

/// Decimal size / rate with 3 significant digits: "12.3 MB/s", "512 B/s".
pub fn dfmt(v: f32, u: &[&str; 6]) -> F {
    dfmt_sp(v, u, true)
}

fn dfmt_sp(v: f32, u: &[&str; 6], sp: bool) -> F {
    let (x, k) = dunit(v);
    let mut f = F::new();
    let c = |m: f32| (x * m).round() as u32;
    if k == 0 || x >= 99.95 {
        f.int(c(1.0), 0);
    } else if x >= 9.995 {
        f.int(c(10.0) / 10, 0);
        f.push(b'.');
        f.push(b'0' + (c(10.0) % 10) as u8);
    } else {
        f.int(c(100.0) / 100, 0);
        f.push(b'.');
        f.push(b'0' + (c(100.0) / 10 % 10) as u8);
        f.push(b'0' + (c(100.0) % 10) as u8);
    }
    if sp {
        f.push(b' ');
    }
    f.str(u[k]);
    f
}

/// The resolution `dfmt` shows `v` with.
pub fn dstep(v: f32) -> f32 {
    let (x, k) = dunit(v);
    let s = if k == 0 || x >= 99.95 {
        1.0
    } else if x >= 9.995 {
        0.1
    } else {
        0.01
    };
    s * [1.0, 1e3, 1e6, 1e9, 1e12, 1e15][k]
}

/// A round axis value ("10 MB/s", "2.5 kB/s"): whole numbers without decimals.
pub fn dtick(v: f32) -> F {
    let (x, k) = dunit(v);
    let t = (x * 10.0).round() as u32;
    let mut f = F::new();
    f.int(t / 10, 0);
    if t % 10 != 0 {
        f.push(b'.');
        f.push(b'0' + (t % 10) as u8);
    }
    f.push(b' ');
    f.str(RATE[k]);
    f
}

/// Chart scale: the next of 1, 2, 5 × 10^k bytes/s at or above `v` (1 kB/s at least).
pub fn nice125(v: f32) -> f32 {
    let mut s = 1000.0f32;
    let mut k = 0;
    while s < v && s < 1e15 {
        s *= if k % 3 == 1 { 2.5 } else { 2.0 };
        k += 1;
    }
    s
}

/// "1 Gbit/s", "2.5 Gbit/s", "100 Mbit/s".
pub fn link_speed(mbit: u32) -> F {
    let mut f = F::new();
    if mbit >= 1000 {
        f.int(mbit / 1000, 0);
        if mbit % 1000 >= 100 {
            f.push(b'.');
            f.push(b'0' + (mbit % 1000 / 100) as u8);
        }
        f.str(" Gbit/s");
    } else {
        f.int(mbit, 0);
        f.str(" Mbit/s");
    }
    f
}

/// "30s ago", "2m ago", "1m 30s ago".
fn ago(s: usize) -> F {
    let mut f = F::new();
    if s >= 60 {
        f.int((s / 60) as u32, 0);
        f.push(b'm');
        if s % 60 > 0 {
            f.push(b' ');
        }
    }
    if s % 60 > 0 {
        f.int((s % 60) as u32, 0);
        f.push(b's');
    }
    f.str(" ago");
    f
}

impl App {
    // ── layout ─────────────────────────────────────────────────────────────

    /// What the layout depends on: changes trigger a new layout.
    fn net_sig(&self) -> u64 {
        let n = &self.n;
        let up = n.primary().filter(|i| i.up).count();
        mixk(&[
            n.ctrs.len().min(64) as u64,
            n.svcs.len().min(64) as u64,
            up as u64,
            n.primary().count() as u64,
            n.virt.is_empty() as u64,
        ])
    }

    /// Lay out again if the things shown changed (containers came or went...).
    pub fn net_refit(&mut self) -> bool {
        if self.page == 2 && self.nl.sig != self.net_sig() {
            self.fit(self.w, self.h);
            return true;
        }
        false
    }

    /// Panels in `r`: one column (by priority: network, containers, connections,
    /// services), two (network over connections | containers over services) or
    /// three (network | connections | containers over services).
    pub fn net_layout(&self, r: Rect) -> NetLay {
        let n = &self.n;
        let (nc, ns) = (n.ctrs.len(), n.svcs.len());
        let nup = n.primary().filter(|i| i.up).count().max(1);
        let extra = (n.primary().count() > nup) as usize + !n.virt.is_empty() as usize;
        let net_want = 2 + 2 * nup + extra + 1 + 14;
        let conn_want = 4 + 2 + n.listen.len() + n.peers.len().min(8);
        let ctr_want = if nc > 0 { 3 + nc } else { 0 };
        let svc_want = if ns > 0 { 4 + 2 * ns.min(8) } else { 0 };
        let mut l = NetLay {
            sig: self.net_sig(),
            ..Default::default()
        };
        let at = |x: usize, y: usize, w: usize, h: usize| Rect { x, y, w, h };
        // one column split between two panels: the first gets `want` (at most
        // `share` %), the second the rest, both at least `min`
        let split = |x: usize, w: usize, a: usize, b: usize, share: usize| -> (Rect, Rect) {
            match (a > 0, b > 0) {
                (true, false) => (at(x, r.y, w, r.h), Rect::default()),
                (false, true) => (Rect::default(), at(x, r.y, w, r.h)),
                (false, false) => (Rect::default(), Rect::default()),
                _ => {
                    let ha = a
                        .min(r.h * share / 100)
                        .max(r.h.min(8))
                        .min(r.h.saturating_sub(5));
                    (at(x, r.y, w, ha), at(x, r.y + ha, w, r.h - ha))
                }
            }
        };
        let side = nc > 0 || ns > 0;
        if r.w >= 100 && r.h >= 20 {
            if side && r.w >= 240 {
                let (w1, w2) = (r.w * 40 / 100, r.w * 27 / 100);
                l.net = at(r.x, r.y, w1, r.h);
                l.conn = at(r.x + w1, r.y, w2, r.h);
                (l.ctr, l.svc) = split(r.x + w1 + w2, r.w - w1 - w2, ctr_want, svc_want, 60);
            } else if side {
                // (the container table is the widest)
                let w1 = r.w * 48 / 100;
                (l.net, l.conn) = split(r.x, w1, net_want, conn_want, 58);
                (l.ctr, l.svc) = split(r.x + w1, r.w - w1, ctr_want, svc_want, 58);
            } else {
                let w1 = r.w * 58 / 100;
                l.net = at(r.x, r.y, w1, r.h);
                l.conn = at(r.x + w1, r.y, r.w - w1, r.h);
            }
            return l;
        }
        // one column: minimums by priority, then the rest by priority up to each want
        let pans = [
            (2 + nup, net_want),
            (if nc > 0 { 3 } else { 0 }, ctr_want),
            (3, conn_want),
            (if ns > 0 { 3 } else { 0 }, svc_want),
        ];
        let mut hs = [0usize; 4];
        let mut left = r.h;
        for (k, p) in pans.iter().enumerate() {
            if p.0 > 0 && p.0 <= left {
                hs[k] = p.0;
                left -= p.0;
            }
        }
        if hs[0] == 0 {
            hs[0] = r.h;
            left = 0;
        }
        // then, in turn: link details, the rate chart (7 rows at least), the lists up
        // to a few rows each, links down / virtual, the lists in full; the chart
        // takes what is left
        let give = |k: usize, n: usize, hs: &mut [usize; 4], left: &mut usize| {
            if hs[k] > 0 && n <= *left {
                hs[k] += n;
                *left -= n;
            }
        };
        give(0, nup, &mut hs, &mut left);
        let chart = left >= 7;
        if chart {
            give(0, 7, &mut hs, &mut left);
        }
        for k in [1, 2, 3] {
            give(
                k,
                (pans[k].1.saturating_sub(hs[k])).min(4).min(left),
                &mut hs,
                &mut left,
            );
        }
        give(0, extra, &mut hs, &mut left);
        for k in [2, 1, 3] {
            give(
                k,
                pans[k].1.saturating_sub(hs[k]).min(left),
                &mut hs,
                &mut left,
            );
        }
        if !chart {
            give(2, left, &mut hs, &mut left);
        }
        hs[0] += left;
        let mut y = r.y;
        let mut out = [Rect::default(); 4];
        for k in 0..4 {
            if hs[k] > 0 {
                out[k] = at(r.x, y, r.w, hs[k]);
                y += hs[k];
            }
        }
        [l.net, l.ctr, l.conn, l.svc] = out;
        l
    }

    /// Columns of the rate chart in the network panel and seconds per column: at
    /// least 2 minutes, up to 5 on wide charts (at most 2 seconds a column there).
    fn chart_geo(&self) -> (usize, usize) {
        let w = self.nl.net.w.saturating_sub(4 + 10).max(1);
        (w, (w * 2).clamp(120, 300).div_ceil(w).clamp(1, 8))
    }

    // ── animation ──────────────────────────────────────────────────────────

    /// New targets after a sample (`smooth`: values glide; else they jump).
    pub fn net_retarget(&mut self, smooth: bool) {
        if self.page != 2 {
            return;
        }
        let (cols, spc) = self.chart_geo();
        let span = cols * spc;
        let n = &self.n;
        let links: Vec<(u64, [f32; 4], f32)> = n
            .primary()
            .map(|i| {
                let peak = |k: usize| {
                    let h = i.h.as_ref().map(|h| &h[k]);
                    let mut m = if k == 0 { i.rx } else { i.tx };
                    if let Some(h) = h {
                        for j in 0..h.len().min(span) {
                            m = m.max(h.back(j));
                        }
                    }
                    nice125(m)
                };
                (hkey(i.name.as_bytes()), [i.rx, i.tx, peak(0), peak(1)], 0.0)
            })
            .collect();
        let a = &mut self.nv;
        a.links.set(&links, smooth, f32::INFINITY, 0.0);
        a.estab.set(n.estab as f32, smooth);
        a.tw.set(n.tw as f32, smooth);
        let ctrs: Vec<(u64, [f32; 4], f32)> = n
            .ctrs
            .iter()
            .filter(|c| c.ok)
            .map(|c| (c.k, [c.cpu, c.mem, c.rx, c.tx], c.cpu))
            .collect();
        a.ctrs.set(&ctrs, smooth, 1.2, 1.0);
        let svc = |by_mem: bool| -> Vec<(u64, [f32; 4], f32)> {
            n.svcs
                .iter()
                .map(|s| {
                    (
                        s.k,
                        [s.cpu, s.mem, 0.0, 0.0],
                        if by_mem { s.mem } else { s.cpu },
                    )
                })
                .collect()
        };
        a.svc_cpu.set(&svc(false), smooth, 1.2, 1.0);
        a.svc_mem.set(&svc(true), smooth, 1.1, 1e6);
        let u: Vec<(u64, [f32; 4], f32)> = n
            .user
            .iter()
            .map(|s| (s.k, [s.cpu, s.mem, 0.0, 0.0], 0.0))
            .collect();
        a.users.set(&u, smooth, 1.0, 0.0);
        let peers: Vec<(u64, [f32; 4], f32)> = n
            .peers
            .iter()
            .map(|p| (peer_key(p), [p.n as f32, 0.0, 0.0, 0.0], p.n as f32))
            .collect();
        a.peers.set(&peers, smooth, 1.25, 1.0);
    }

    /// Smooth mode: one frame of every network-page spring and digit.
    pub fn net_step(&mut self, dt: f32) -> bool {
        if self.page != 2 {
            return false;
        }
        let ex = (-W_LOAD * dt).exp();
        let a = &mut self.nv;
        let rate = |q: usize, x: f32| if q < 2 { dstep(x) } else { x * 0.002 + 1.0 };
        let res = |q: usize, x: f32| match q {
            0 => 0.1,
            1 => bstep(x),
            _ => dstep(x),
        };
        let cnt = |_: usize, _: f32| 1.0;
        let mut mv = a.links.step(dt, ex, &rate);
        mv |= a.ctrs.step(dt, ex, &res);
        mv |= a.svc_cpu.step(dt, ex, &res);
        mv |= a.svc_mem.step(dt, ex, &res);
        mv |= a.users.step(dt, ex, &res);
        mv |= a.peers.step(dt, ex, &cnt);
        for v in [&mut a.estab, &mut a.tw] {
            mv |= v.step(dt, W_LOAD, ex, 0.25);
            mv |= v.digits(dt, 1.0, 0.0);
        }
        mv
    }

    fn nmemo(&mut self, slot: usize, key: u64) -> bool {
        if self.ncache.len() <= slot {
            self.ncache.resize(slot + 1, u64::MAX);
        }
        let ch = self.ncache[slot] != key;
        self.ncache[slot] = key;
        ch
    }

    /// Title-bar summary: "eno1 1 Gbit/s · 5 containers · 16 services".
    pub fn net_info(&self) -> (F, F) {
        let n = &self.n;
        let mut a = F::new();
        if let Some(i) = n.primary().find(|i| i.up) {
            a.str("   ");
            a.str(&i.name);
            if i.speed > 0 {
                a.push(b' ');
                a.str(link_speed(i.speed).s());
            }
        }
        let mut b = F::new();
        for (c, w) in [(n.ctrs.len(), " container"), (n.svcs.len(), " service")] {
            if c > 0 {
                b.str(" · ");
                b.int(c as u32, 0);
                b.str(w);
                if c > 1 {
                    b.push(b's');
                }
            }
        }
        (a, b)
    }

    // ── drawing ────────────────────────────────────────────────────────────

    /// A table row: cells (text, colour, width): `w > 0` left-aligned and clipped in
    /// w cells, `w < 0` right-aligned in -w cells, 0 as it is; then blanks to `maxx`.
    fn row(scr: &mut Screen, mut x: usize, y: usize, maxx: usize, cells: &[(&str, u32, i32)]) {
        let bg = surf(S_PANEL);
        for &(s, c, w) in cells {
            let wa = w.unsigned_abs() as usize;
            if w < 0 {
                for _ in s.chars().count()..wa {
                    if x < maxx {
                        scr.put(x, y, ' ', 0, bg);
                    }
                    x += 1;
                }
            }
            let end = if w > 0 { (x + wa).min(maxx) } else { maxx };
            x = scr.text(x, y, s, c, bg, end);
            while w > 0 && x < end {
                scr.put(x, y, ' ', 0, bg);
                x += 1;
            }
        }
        while x < maxx {
            scr.put(x, y, ' ', 0, bg);
            x += 1;
        }
    }

    fn blank(scr: &mut Screen, x: usize, y: &mut usize, end: usize, maxx: usize) {
        while *y < end {
            Self::row(scr, x, *y, maxx, &[]);
            *y += 1;
        }
    }

    /// The network page. `graphs`: also the chart (not on bars-only frames).
    pub fn draw_net(&mut self, scr: &mut Screen, full: bool, graphs: bool) {
        if full {
            self.ncache.iter_mut().for_each(|c| *c = u64::MAX);
        }
        let l = self.nl;
        if l.net.w > 0 && (graphs || full) {
            self.draw_links(scr, l.net, full, graphs);
        }
        // (bars-only frames: only the panels with bars)
        if l.conn.w > 0 && (graphs || full) {
            self.draw_conn(scr, l.conn, full);
        }
        if l.ctr.w > 0 {
            self.draw_ctrs(scr, l.ctr, full);
        }
        if l.svc.w > 0 {
            self.draw_svcs(scr, l.svc, full);
        }
        if l.soon.w > 0 && full {
            let th = &self.t;
            self.panel(scr, l.soon, &[("processes", th.title | BOLD)], "");
            let ir = l.soon.inner();
            let s = "Processes — coming soon";
            let x = ir.x + ir.w.saturating_sub(s.chars().count()) / 2;
            scr.text(x, ir.y + ir.h / 2, s, th.mid, surf(S_PANEL), ir.x + ir.w);
        }
    }

    /// Shown rate values of a link (smooth: gliding), chart scales.
    fn link_vals(&self, name: &str) -> [Val; 4] {
        let k = hkey(name.as_bytes());
        self.nv
            .links
            .it
            .iter()
            .find(|i| i.key == k)
            .map_or([Val::default(); 4], |i| [i.v[0], i.v[1], i.v[2], i.v[3]])
    }

    fn draw_links(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let now = crate::term::mono();
        let n = &self.n;
        let ups: Vec<usize> = (0..n.ifs.len())
            .filter(|&i| n.ifs[i].prim && n.ifs[i].up)
            .collect();
        let downs: Vec<usize> = (0..n.ifs.len())
            .filter(|&i| n.ifs[i].prim && !n.ifs[i].up)
            .collect();
        // rows: a rate line per link up; then (keeping 8 rows for the chart) details,
        // links down, virtual links, a gap; the chart gets the rest
        let mut rem = ih.saturating_sub(ups.len().max(1));
        let res = if rem >= 7 { 7 } else { 0 };
        let det = !ups.is_empty() && rem >= res + ups.len();
        if det {
            rem -= ups.len();
        }
        let dl = !downs.is_empty() && rem > res;
        rem -= dl as usize;
        let vl = !n.virt.is_empty() && rem > res;
        rem -= vl as usize;
        let gap = res > 0 && rem >= 9;
        let ch = if res > 0 { rem - gap as usize } else { 0 };
        let main = ups.first().copied().or(n.ifs.iter().position(|i| i.prim));
        // chart rows: title, download body, title, upload body, time axis
        // (odd heights: the middle label's row is exactly half the scale)
        let body = ch.saturating_sub(3);
        let hu = (body / 2).saturating_sub((body / 2 + 1) % 2 * (body / 2 > 1) as usize);
        let hd = (body - hu).saturating_sub((body - hu + 1) % 2 * (body - hu > 1) as usize);
        let ch = if ch > 0 { hd + hu + 3 } else { 0 };
        let y_chart = ir.y + ih - ch;
        if full {
            let sub = if ch > 0 && main.is_some() {
                "speed now · over time"
            } else {
                ""
            };
            self.panel(scr, r, &[("network", self.t.title | BOLD)], sub);
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let mut k = mixk(&[iw as u64, ih as u64, n.ifs.len() as u64]);
        for &i in ups.iter().chain(&downs) {
            let f = &n.ifs[i];
            let v = self.link_vals(&f.name);
            k = mixk(&[
                k,
                v[0].n.to_bits() as u64,
                v[1].n.to_bits() as u64,
                f.addr.len() as u64,
                f.sig as u64,
                f.bad(now) as u64,
                (f.cnt[0] / 10_000_000) ^ (f.cnt[1] / 10_000_000) << 32,
                f.cnt[3] ^ f.cnt[2] << 32,
                f.speed as u64,
            ]);
        }
        let maxx = ir.x + iw;
        let (cd, cu) = (self.t.accent, self.t.accent2);
        if self.nmemo(N_TEXT, k) || full {
            let th = &self.t;
            let n = &self.n;
            let b = th.fg | BOLD;
            let mut y = ir.y;
            if ups.is_empty() {
                Self::row(scr, ir.x, y, maxx, &[("no network connection", th.warn, 0)]);
                y += 1;
            }
            let kind = |f: &Iface| fc(&[" (", KIND[f.kind as usize], ")"]);
            let nw = ups
                .iter()
                .map(|&i| n.ifs[i].name.chars().count() + kind(&n.ifs[i]).n)
                .max()
                .unwrap_or(0);
            let wide = iw >= nw + 43;
            for &i in &ups {
                let f = &n.ifs[i];
                let v = self.link_vals(&f.name);
                let (d, u) = (dfmt(v[0].n, &RATE), dfmt(v[1].n, &RATE));
                let kd = kind(f);
                if wide {
                    let kw = (nw + 2 - f.name.chars().count()) as i32;
                    Self::row(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        &[
                            (&f.name, b, 0),
                            (kd.s(), th.mid, kw),
                            ("↓", cd, 0),
                            (d.s(), b, -10),
                            (" download   ", th.dim, 0),
                            ("↑", cu, 0),
                            (u.s(), b, -10),
                            (" upload", th.dim, 0),
                        ],
                    );
                } else {
                    Self::row(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        &[
                            (&f.name, b, 0),
                            (" ↓", cd, 0),
                            (d.s(), b, -10),
                            (" ↑", cu, 0),
                            (u.s(), b, -10),
                        ],
                    );
                }
                y += 1;
                if det {
                    let mut d = F::new();
                    if f.speed > 0 {
                        d.str(link_speed(f.speed).s());
                        d.str(if iw >= 66 { " link · " } else { " · " });
                    }
                    if !f.addr.is_empty() {
                        d.str(&f.addr);
                        d.str(" · ");
                    }
                    d.str("since boot ↓");
                    d.str(dfmt(f.cnt[0] as f32, &SIZE).s());
                    d.str(" ↑");
                    d.str(dfmt(f.cnt[1] as f32, &SIZE).s());
                    // Wi-Fi signal, and problems: in colour only while they happen
                    let (mut w, mut wc) = (F::new(), th.mid);
                    if f.kind == K_WIFI && f.sig != 0 {
                        w.str(" · signal −");
                        w.int(f.sig.unsigned_abs(), 0);
                        w.str(" dBm ");
                        w.str(if f.sig >= -60 {
                            "good"
                        } else if f.sig >= -70 {
                            "fair"
                        } else {
                            wc = th.warn;
                            "weak"
                        });
                    }
                    let mut p = F::new();
                    for (c, s) in [(f.cnt[3], " dropped"), (f.cnt[2], " errors")] {
                        if c > 0 {
                            p.str(" · ");
                            p.str(dshort(c as f32).s());
                            p.str(s);
                        }
                    }
                    let pc = if f.bad(now) { th.warn } else { th.dim };
                    // (all of it or none: never a cut-off count)
                    let len = |f: &F| f.s().chars().count();
                    if 1 + len(&d) + len(&w) + len(&p) > iw {
                        p = F::new();
                    }
                    Self::row(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        &[
                            (" ", 0, 0),
                            (d.s(), th.mid, 0),
                            (w.s(), wc, 0),
                            (p.s(), pc, 0),
                        ],
                    );
                    y += 1;
                }
            }
            if dl {
                let mut s = F::new();
                for &i in &downs {
                    if s.n > 0 {
                        s.str(" · ");
                    }
                    s.str(&n.ifs[i].name);
                    s.str(kind(&n.ifs[i]).s());
                }
                Self::row(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    &[("not connected: ", th.dim, 0), (s.s(), th.mid, 0)],
                );
                y += 1;
            }
            if vl {
                Self::row(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    &[("virtual: ", th.dim, 0), (&n.virt, th.mid, 0)],
                );
                y += 1;
            }
            Self::blank(scr, ir.x, &mut y, y_chart, maxx);
            // chart titles, the legend: series, link, the value now, time span
            if let (true, Some(m)) = (ch > 0, main) {
                let f = &n.ifs[m];
                let v = self.link_vals(&f.name);
                let (cols, spc) = self.chart_geo();
                let win = span(cols * spc);
                for (j, y, c, w, val) in [
                    (0, y_chart, cd, "download", v[0].n),
                    (1, y_chart + 1 + hd, cu, "upload", v[1].n),
                ] {
                    let nv = dfmt(val, &RATE);
                    let what = fc(&[w, " on ", &f.name]);
                    let tail = if j == 0 {
                        fc(&[" · last ", win.s()])
                    } else {
                        F::new()
                    };
                    Self::row(
                        scr,
                        ir.x,
                        y,
                        maxx,
                        &[
                            ("■ ", c, 0),
                            (what.s(), th.text, 0),
                            ("  now ", th.dim, 0),
                            (nv.s(), b, 0),
                            (tail.s(), th.dim, 0),
                        ],
                    );
                }
            }
        }
        if ch == 0 || hd == 0 || !(graphs || full) {
            return;
        }
        let Some(m) = main else { return };
        let (cols, spc) = self.chart_geo();
        let v = self.link_vals(&self.n.ifs[m].name);
        let body = if self.smooth() {
            self.body_key(0)
        } else {
            self.n.hist_n
        };
        let ck = mixk(&[
            body,
            iw as u64,
            ih as u64,
            v[2].x.to_bits() as u64,
            v[3].x.to_bits() as u64,
        ]);
        if self.nmemo(N_CHART, ck) || full {
            let mut vals = std::mem::take(&mut self.vals);
            let (pr, pt) = self.n.ifs[m].part();
            for (k, y, h, c, head, part, sc) in [
                (0, y_chart + 1, hd, cd, v[0].x, pr, v[2].x),
                (1, y_chart + 2 + hd, hu, cu, v[1].x, pt, v[3].x),
            ] {
                vals.clear();
                if let Some(hist) = &self.n.ifs[m].h {
                    self.rate_cols(&mut vals, cols, spc, &hist[k], head, part);
                }
                self.rate_chart(scr, ir.x, y, iw, h, &vals, sc.max(1.0), c);
            }
            self.vals = vals;
        }
        if self.nmemo(N_AXIS, mixk(&[iw as u64, ih as u64, cols as u64])) || full {
            self.time_axis(scr, ir.x, ir.y + ih - 1, iw, cols, spc);
        }
    }

    /// `n` chart columns `spc` seconds each (oldest first): bucket means of the
    /// history, aligned to whole buckets so they don't wobble; smooth: the scrolling
    /// curve with the live value at its head.
    fn rate_cols(
        &self,
        out: &mut Vec<f32>,
        n: usize,
        spc: usize,
        h: &crate::sys::Ring<f32>,
        head: f32,
        part: f32,
    ) {
        out.clear();
        if self.smooth() {
            let mut raw = std::mem::take(&mut *self.scratch.borrow_mut());
            crate::anim::Curve {
                phase: self.phase,
                head,
                part,
                len: h.len(),
                pt: &|k| h.back(k),
                pad: 0.0,
            }
            .sample(&mut raw, n * spc, 0.0, 1.0);
            out.extend(raw.chunks(spc).map(|c| c.iter().sum::<f32>() / spc as f32));
            *self.scratch.borrow_mut() = raw;
            return;
        }
        let last = (self.n.pts as usize).saturating_sub(1);
        out.resize(n, 0.0);
        for j in 0..n.min(last / spc + 1) {
            let b = last / spc - j;
            let (mut s, mut c) = (0.0, 0);
            for idx in b * spc..((b + 1) * spc).min(last + 1) {
                if last - idx < h.len() {
                    s += h.back(last - idx);
                    c += 1;
                }
            }
            out[n - 1 - j] = s / c.max(1) as f32;
        }
    }

    /// Columns chart (eighth blocks) of `vals` (one per column, newest right) with a
    /// y axis gutter: the scale at the top, half of it half way, 0 at the bottom.
    #[allow(clippy::too_many_arguments)]
    fn rate_chart(
        &self,
        scr: &mut Screen,
        x0: usize,
        y0: usize,
        w: usize,
        h: usize,
        vals: &[f32],
        vmax: f32,
        col: u32,
    ) {
        const BL: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let bg = surf(S_PANEL);
        let gut = 10;
        let cw = w.saturating_sub(gut);
        for r in 0..h {
            let lab = if r == 0 {
                dtick(vmax)
            } else if r == h / 2 && h >= 4 {
                dtick(vmax / 2.0)
            } else {
                F::new()
            };
            Self::row(
                scr,
                x0,
                y0 + r,
                x0 + gut,
                &[(lab.s(), self.t.dim, 1 - gut as i32)],
            );
            let grid = lab.n > 0 && r + 1 < h;
            let base = (h - 1 - r) * 8;
            for i in 0..cw {
                let v = vals
                    .get((vals.len() + i).wrapping_sub(cw))
                    .copied()
                    .unwrap_or(0.0);
                let e = if v > 0.0 {
                    ((v / vmax * (h * 8) as f32).round() as usize).clamp(1, h * 8)
                } else {
                    0
                };
                let (c, fg) = if e >= base + 8 {
                    ('█', col)
                } else if e > base {
                    (BL[e - base - 1], col)
                } else if grid && i % 2 == 0 {
                    ('·', self.t.border)
                } else {
                    (' ', NOPATCH)
                };
                scr.put(x0 + gut + i, y0 + r, c, fg, bg);
            }
        }
    }

    /// Time axis under the chart: "2m ago … 1m ago … now".
    fn time_axis(&self, scr: &mut Screen, x0: usize, y: usize, w: usize, cols: usize, spc: usize) {
        let bg = surf(S_PANEL);
        let th = &self.t;
        let right = x0 + w;
        Self::row(scr, x0, y, right, &[("now", th.mid, -(w as i32))]);
        let gx = right.saturating_sub(cols);
        let step = [10, 15, 30, 60, 120, 300, 600, 900, 1800]
            .into_iter()
            .find(|&t| t / spc >= 14)
            .unwrap_or(3600);
        let mut lim = right - 4;
        let mut a = step;
        while a / spc < cols {
            let cx = right - 1 - a / spc;
            let s = ago(a);
            let l = s.s().chars().count();
            let x = (cx + 1).saturating_sub(l / 2 + 1);
            if x + l < lim && x >= gx {
                scr.text(x, y, s.s(), th.dim, bg, right);
                lim = x;
            }
            a += step;
        }
    }

    fn draw_conn(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let (a, n) = (&self.nv, &self.n);
        let unk = n.listen.iter().any(|l| l.owner == "?");
        let mut k = mixk(&[
            iw as u64,
            ih as u64,
            a.estab.n.to_bits() as u64,
            a.tw.n.to_bits() as u64,
            n.sock_ok as u64,
            a.peers.key(1, 1.0),
        ]);
        for l in &n.listen {
            k = mixk(&[
                k,
                l.port as u64,
                l.proto as u64,
                l.scope as u64,
                hkey(l.owner.as_bytes()),
            ]);
        }
        if !(self.nmemo(N_CONN, k) || full) {
            return;
        }
        let (a, n, th) = (&self.nv, &self.n, &self.t);
        if full {
            let sub = if unk && iw >= 44 {
                "? = another user's program"
            } else {
                ""
            };
            self.panel(scr, r, &[("connections & ports", th.title | BOLD)], sub);
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let maxx = ir.x + iw;
        let mut y = ir.y;
        let end = ir.y + ih;
        let (b, d) = (th.fg | BOLD, th.dim);
        let (e, t, np) = (
            num("", a.estab.n, 0, ""),
            num("", a.tw.n, 0, ""),
            num("", n.listen.len() as f32, 0, ""),
        );
        let (w1, w2, w3) = if iw >= 76 {
            (
                " open connections · ",
                " closed in the last minute · ",
                " ports listening",
            )
        } else if iw >= 50 {
            (" open · ", " closed in the last minute · ", " listening")
        } else {
            (" open · ", " closing · ", " ports")
        };
        Self::row(
            scr,
            ir.x,
            y,
            maxx,
            &[
                (e.s(), b, 0),
                (w1, d, 0),
                (t.s(), th.text, 0),
                (w2, d, 0),
                (np.s(), b, 0),
                (w3, d, 0),
            ],
        );
        y += 1;
        if !n.sock_ok {
            Self::blank(scr, ir.x, &mut y, end, maxx);
            return;
        }
        // listening ports, then the busiest remote ends
        let rows = end - y;
        let nl = n.listen.len();
        let npe = a.peers.it.len().min(8);
        let pr = if npe == 0 {
            rows
        } else {
            (nl + 2).min(rows.saturating_sub((npe + 2).min(rows * 2 / 5)))
        };
        let pw = if iw >= 52 { 9 } else { 0 };
        let sw = if iw >= 40 { 20.min(iw / 3) as i32 } else { 0 };
        let lim = y + pr;
        if nl == 0 && lim > y + 1 {
            Self::row(scr, ir.x, y, maxx, &[]);
            Self::row(
                scr,
                ir.x,
                y + 1,
                maxx,
                &[("no program is listening for connections", d, 0)],
            );
            y += 2;
        } else if lim > y + 3 {
            y += 1;
            Self::row(scr, ir.x, y - 1, maxx, &[]);
            Self::row(
                scr,
                ir.x,
                y,
                maxx,
                &[
                    ("port", d, -5),
                    ("  ", 0, 0),
                    (if pw > 0 { "protocol" } else { "" }, d, pw),
                    (if sw > 0 { "listens on" } else { "" }, d, sw),
                    ("program", d, 0),
                ],
            );
            y += 1;
            let show = lim - y;
            for (i, l) in n.listen.iter().enumerate() {
                if i + 1 == show && nl > show {
                    let more = fc(&["+ ", num("", (nl - i) as f32, 0, " more").s()]);
                    Self::row(scr, ir.x, y, maxx, &[(more.s(), d, 0)]);
                    y += 1;
                    break;
                }
                if y >= lim {
                    break;
                }
                let p = num("", l.port as f32, 0, "");
                let only = fc(&[&l.addr, " only"]);
                let scope = match l.scope {
                    0 => "all addresses",
                    1 => "this computer only",
                    _ => only.s(),
                };
                Self::row(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    &[
                        (p.s(), b, -5),
                        ("  ", 0, 0),
                        (
                            if pw > 0 {
                                ["", "tcp", "udp", "tcp+udp"][l.proto as usize & 3]
                            } else {
                                ""
                            },
                            th.mid,
                            pw,
                        ),
                        (
                            if sw > 0 { scope } else { "" },
                            if l.scope == 0 { th.text } else { th.mid },
                            sw,
                        ),
                        (&l.owner, if l.owner == "?" { d } else { th.fg }, 0),
                    ],
                );
                y += 1;
            }
        }
        Self::blank(scr, ir.x, &mut y, lim.min(end), maxx);
        if npe > 0 && end > y + 2 {
            Self::row(scr, ir.x, y, maxx, &[]);
            Self::row(
                scr,
                ir.x,
                y + 1,
                maxx,
                &[("most connections to (remote end)", d, 0)],
            );
            y += 2;
            for it in a.peers.order() {
                if y >= end {
                    break;
                }
                let Some(p) = n.peers.iter().find(|p| peer_key(p) == it.key) else {
                    continue;
                };
                let ip = fmt_ip(&p.ip, p.v6);
                let ps = num(":", p.port as f32, 0, "");
                let host = fc(&[if p.local { "this computer" } else { &ip }, ps.s(), " "]);
                let owner = n
                    .ctr_by_ip(&ip)
                    .or_else(|| p.local.then(|| n.ctr_by_port(p.port)).flatten())
                    .or_else(|| {
                        let l = n
                            .listen
                            .iter()
                            .find(|l| p.local && l.port == p.port && l.owner != "?");
                        l.map(|l| l.owner.as_str())
                    });
                let wf = fc(&["→ ", owner.unwrap_or("")]);
                let who = if owner.is_some() {
                    wf.s()
                } else {
                    port_name(p.port)
                };
                let cnt = num("", it.v[0].n, 0, "");
                let ww = iw.saturating_sub(host.s().chars().count() + 6) as i32;
                Self::row(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    &[
                        (host.s(), th.text, 0),
                        (who, th.mid, ww.max(1)),
                        (cnt.s(), th.fg, -6),
                    ],
                );
                y += 1;
            }
        }
        Self::blank(scr, ir.x, &mut y, end, maxx);
    }

    /// Rows of a usage list from `y` to `end`: name, a status word (containers'
    /// health), CPU now as a bar and %, memory and (`net`) network ↓ / ↑. `get(key)`:
    /// name, status word and its colour, shares the host's network.
    #[allow(clippy::too_many_arguments)]
    fn usage_rows<'a>(
        &self,
        scr: &mut Screen,
        ir: Rect,
        mut y: usize,
        end: usize,
        list: &List,
        hdr: &str,
        health: bool,
        net: bool,
        get: &dyn Fn(u64) -> Option<(&'a str, &'a str, u32, bool)>,
    ) -> usize {
        let th = &self.t;
        let (iw, maxx) = (ir.w, ir.x + ir.w);
        // widths: the name first, then health, CPU %, memory, network, the CPU bar
        let hw = if health && iw >= 40 { 10 } else { 0 };
        let long = list
            .it
            .iter()
            .filter_map(|i| Some(get(i.key)?.0.chars().count()))
            .max()
            .unwrap_or(4)
            + 1;
        let fixed = hw + 6 + 9;
        let mut left = iw.saturating_sub(fixed + long.clamp(8, 24));
        let netw = if net && left >= 16 { 16 } else { 0 };
        left -= netw;
        let bw = if left >= 5 { left.min(12) } else { 0 };
        let nw = iw.saturating_sub(fixed + netw + bw).min(32);
        let d = th.dim;
        if y + 1 < end && !hdr.is_empty() {
            Self::row(
                scr,
                ir.x,
                y,
                maxx,
                &[
                    (hdr, d, nw as i32),
                    (if hw > 0 { "health" } else { "" }, d, hw as i32),
                    (if bw > 1 { "cpu now" } else { "cpu" }, d, -(bw as i32 + 6)),
                    ("memory", d, -9),
                    (
                        if netw > 0 { "↓in ↑out per s" } else { "" },
                        d,
                        -(netw as i32),
                    ),
                ],
            );
            y += 1;
        }
        let top_mem = list.it.iter().map(|i| i.v[1].x).fold(0.0, f32::max);
        for (row, it) in list.order().enumerate() {
            if y >= end {
                break;
            }
            let Some((name, st, sc, host)) = get(it.key) else {
                continue;
            };
            let v = &it.v;
            let xb = ir.x + nw + hw;
            Self::row(
                scr,
                ir.x,
                y,
                xb,
                &[
                    (name, if row == 0 { th.fg } else { th.text }, nw as i32),
                    (st, sc, 0),
                ],
            );
            if bw > 1 {
                self.meter(
                    scr,
                    xb,
                    y,
                    v[0].x / 100.0,
                    bw - 1,
                    &th.freq,
                    surf(S_PANEL),
                    LOAD,
                );
                scr.put(xb + bw - 1, y, ' ', 0, surf(S_PANEL));
            }
            let p = pct1(v[0].n / 100.0);
            let m = bsize(v[1].n, false);
            let (rx, tx) = (
                fc(&["↓", dshort(v[2].n).s()]),
                fc(&[" ↑", dshort(v[3].n).s()]),
            );
            let bold = |b: bool| if b { th.fg | BOLD } else { th.text };
            Self::row(
                scr,
                xb + bw,
                y,
                maxx,
                &[
                    (p.s(), bold(row == 0 && v[0].n >= 1.0), -6),
                    (m.s(), bold(v[1].x >= top_mem && list.it.len() > 1), -9),
                    (
                        if netw == 0 {
                            ""
                        } else if host {
                            "   host network"
                        } else {
                            rx.s()
                        },
                        th.mid,
                        if netw > 0 { -8 } else { 0 },
                    ),
                    (
                        if netw == 0 || host { "" } else { tx.s() },
                        th.mid,
                        if netw > 0 { -8 } else { 0 },
                    ),
                ],
            );
            y += 1;
        }
        y
    }

    fn draw_ctrs(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let n = &self.n;
        let nrest = n.api.iter().filter(|c| c.state == 1).count();
        let k = mixk(&[
            iw as u64,
            ih as u64,
            self.nv.ctrs.key(12, 100.0),
            n.stopped as u64,
            nrest as u64,
            n.api_err as u64,
        ]);
        if !(self.nmemo(N_CTR, k) || full) {
            return;
        }
        let (n, th) = (&self.n, &self.t);
        let nr = n.ctrs.len();
        if full {
            let c = num(" ", nr as f32, 0, " running");
            let s = num(" · ", n.stopped as f32, 0, " stopped");
            let title: [(&str, u32); 3] = [
                ("containers", th.title | BOLD),
                (c.s(), th.text),
                (if n.stopped > 0 { s.s() } else { "" }, th.dim),
            ];
            let sub = if n.api_err && iw >= 50 {
                "cpu 100% = 1 core · no docker access"
            } else if iw >= 30 {
                "cpu: 100% = one core"
            } else {
                ""
            };
            self.panel(scr, r, &title, sub);
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let maxx = ir.x + iw;
        let end = ir.y + ih;
        let find = |k: u64| n.ctrs.iter().find(|c| c.k == k);
        if ih == 1 {
            // tiny: the count and the busiest
            let c = num("", nr as f32, 0, " running");
            let top = self.nv.ctrs.order().next().and_then(|i| find(i.key));
            let p = top.map_or(F::new(), |t| pct1(t.cpu / 100.0));
            Self::row(
                scr,
                ir.x,
                ir.y,
                maxx,
                &[
                    (c.s(), th.fg, 0),
                    (if top.is_some() { " · busiest " } else { "" }, th.dim, 0),
                    (top.map_or("", |t| &t.name), th.text, 0),
                    (" ", 0, 0),
                    (p.s(), th.text, 0),
                ],
            );
            return;
        }
        let get = |k: u64| {
            let c = find(k)?;
            let (s, col) = match (c.state, c.health) {
                (1, _) => ("restarting", th.warn),
                (2, _) => ("paused", th.dim),
                (_, 3) => ("unhealthy", th.crit | BOLD),
                (_, 2) => ("starting", th.mid),
                (_, 1) => ("healthy", th.mid),
                _ => ("running", th.dim),
            };
            Some((c.name.as_str(), s, col, c.host_net))
        };
        let mut y = self.usage_rows(scr, ir, ir.y, end, &self.nv.ctrs, "name", true, true, &get);
        // restarting containers have no cgroup to read: listed by the API only
        for c in n
            .api
            .iter()
            .filter(|c| c.state == 1 && !n.ctrs.iter().any(|x| x.id == c.id))
        {
            if y >= end {
                break;
            }
            Self::row(
                scr,
                ir.x,
                y,
                maxx,
                &[(&c.name, th.text, 24), ("restarting", th.warn, 0)],
            );
            y += 1;
        }
        Self::blank(scr, ir.x, &mut y, end, maxx);
    }

    fn draw_svcs(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let a = &self.nv;
        let k = mixk(&[
            iw as u64,
            ih as u64,
            a.svc_cpu.key(12, 100.0),
            a.svc_mem.key(0, 1.0),
            a.users.key(12, 100.0),
        ]);
        if !(self.nmemo(N_SVC, k) || full) {
            return;
        }
        let (a, n, th) = (&self.nv, &self.n, &self.t);
        if full {
            let c = num(" ", n.svcs.len() as f32, 0, "");
            let title: [(&str, u32); 3] = [
                ("services", th.title | BOLD),
                (c.s(), th.text),
                (if n.has_systemd { " (systemd)" } else { "" }, th.dim),
            ];
            let sub = if iw >= 30 { "cpu: 100% = one core" } else { "" };
            self.panel(scr, r, &title, sub);
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let (maxx, end) = (ir.x + iw, ir.y + ih);
        let get = |k: u64| {
            let s = n.svcs.iter().chain(n.user.iter()).find(|s| s.k == k)?;
            let user = s.name == "user.slice";
            Some((
                if user {
                    "programs of logged-in users"
                } else {
                    s.name.as_str()
                },
                "",
                0,
                false,
            ))
        };
        // what the logged-in users run (not a service: their sessions and apps), then
        // two lists when there is room: busiest (CPU) and largest (memory)
        let mut y = ir.y;
        if !a.users.it.is_empty() && ih >= 6 {
            y = self.usage_rows(scr, ir, y, y + 1, &a.users, "", false, false, &get);
            Self::row(scr, ir.x, y, maxx, &[]);
            y += 1;
        }
        let two = end - y >= 12 && a.svc_mem.it.len() > 3;
        let lim = if two { y + (end - y - 1) / 2 } else { end };
        let mut y = self.usage_rows(
            scr,
            ir,
            y,
            lim,
            &a.svc_cpu,
            "services, busiest now",
            false,
            false,
            &get,
        );
        Self::blank(scr, ir.x, &mut y, lim, maxx);
        if two {
            Self::row(scr, ir.x, y, maxx, &[]);
            y = self.usage_rows(
                scr,
                ir,
                y + 1,
                end,
                &a.svc_mem,
                "services, most memory",
                false,
                false,
                &get,
            );
        }
        Self::blank(scr, ir.x, &mut y, end, maxx);
    }
}

/// Concatenated (cheaper in code than formatting).
fn fc(p: &[&str]) -> F {
    let mut f = F::new();
    for s in p {
        f.str(s);
    }
    f
}

fn peer_key(p: &Peer) -> u64 {
    let mut k = [0u8; 18];
    k[..16].copy_from_slice(&p.ip);
    k[16..].copy_from_slice(&p.port.to_ne_bytes());
    hkey(&k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_formatting() {
        assert_eq!(dfmt(12.34e6, &RATE).s(), "12.3 MB/s");
        assert_eq!(dfmt(512.0, &RATE).s(), "512 B/s");
        assert_eq!(dfmt(1.5e9, &SIZE).s(), "1.50 GB");
        assert_eq!(dfmt(60.3365e9, &SIZE).s(), "60.3 GB");
        assert_eq!(dstep(12.3e6), 1e5);
        assert_eq!(dtick(10e6).s(), "10 MB/s");
        assert_eq!(dtick(2.5e3).s(), "2.5 kB/s");
        assert_eq!(nice125(0.0), 1000.0);
        assert_eq!(nice125(3e6), 5e6);
        assert_eq!(nice125(5.1e6), 1e7);
        assert_eq!(nice125(1.1e7), 2e7);
        assert_eq!(link_speed(1000).s(), "1 Gbit/s");
        assert_eq!(link_speed(2500).s(), "2.5 Gbit/s");
        assert_eq!(link_speed(100).s(), "100 Mbit/s");
        assert_eq!(ago(90).s(), "1m 30s ago");
        assert_eq!(ago(120).s(), "2m ago");
    }

    #[test]
    fn network_page_fits_at_all_sizes() {
        let rows = |scr: &Screen, y: usize| -> String {
            (0..scr.w).map(|x| scr.buf[y * scr.w + x].ch).collect()
        };
        for (nc, ns) in [(5, 16), (0, 0), (0, 5), (30, 0), (2, 60)] {
            for w in (20..=420).step_by(40) {
                for h in (6..=110).step_by(13) {
                    let s = crate::sys::Sampler::new_for_test(&(0..8).collect::<Vec<_>>());
                    let mut a = App::new(s, crate::theme::load("default").unwrap());
                    a.n = crate::net::Net::fake(nc, ns);
                    a.page = 2;
                    a.first_size(w, h);
                    a.net_retarget(false);
                    let rs: Vec<Rect> = a.nl.rects().into_iter().filter(|r| r.w > 0).collect();
                    assert!(!rs.is_empty() || h < 5, "{w}x{h}: nothing shown");
                    for (i, r) in rs.iter().enumerate() {
                        assert!(r.x + r.w <= w && r.y >= 1 && r.y + r.h < h, "{w}x{h} {r:?}");
                        for q in &rs[i + 1..] {
                            let apart = r.x + r.w <= q.x
                                || q.x + q.w <= r.x
                                || r.y + r.h <= q.y
                                || q.y + q.h <= r.y;
                            assert!(apart, "{w}x{h} overlap {r:?} {q:?}");
                        }
                    }
                    let mut scr = Screen::new(w, h);
                    a.draw(&mut scr, true);
                    a.speed = SMOOTH;
                    a.net_retarget(true);
                    a.net_step(1.0 / 60.0);
                    a.draw(&mut scr, false);
                    a.draw_bars(&mut scr);
                    // the essentials at 40x12: rates and the container count
                    if (w, h) == (60, 19) && nc > 0 {
                        let all: String = (0..h).map(|y| rows(&scr, y)).collect();
                        assert!(all.contains("↓") && all.contains("running"), "{all}");
                    }
                }
            }
        }
        // the primary size: every panel, the chart with its time axis
        let s = crate::sys::Sampler::new_for_test(&(0..8).collect::<Vec<_>>());
        let mut a = App::new(s, crate::theme::load("default").unwrap());
        a.n = crate::net::Net::fake(5, 16);
        a.page = 2;
        a.first_size(128, 45);
        a.net_retarget(false);
        let mut scr = Screen::new(128, 45);
        a.draw(&mut scr, true);
        let all: String = (0..45)
            .map(|y| rows(&scr, y))
            .collect::<Vec<_>>()
            .join("\n");
        for s in [
            "download",
            "upload",
            "now",
            "ago",
            "listening",
            "containers",
            "services",
            "MB/s",
        ] {
            assert!(all.contains(s), "{s} missing:\n{all}");
        }
    }

    #[test]
    fn lists_keep_their_order_unless_clearly_overtaken() {
        let mut l = List::default();
        let d = |a: f32, b: f32| vec![(1, [a, 0.0, 0.0, 0.0], a), (2, [b, 0.0, 0.0, 0.0], b)];
        l.set(&d(10.0, 5.0), false, 1.2, 1.0);
        assert_eq!(l.order().next().unwrap().key, 1);
        // close: no flip-flop
        l.set(&d(10.0, 11.0), false, 1.2, 1.0);
        assert_eq!(l.order().next().unwrap().key, 1);
        l.set(&d(10.0, 20.0), false, 1.2, 1.0);
        assert_eq!(l.order().next().unwrap().key, 2);
        // smooth: positions glide to the new order
        l.set(&d(30.0, 20.0), true, 1.2, 1.0);
        assert_eq!(l.order().next().unwrap().key, 2);
        let ex = (-W_LOAD / 60.0f32).exp();
        for _ in 0..60 {
            l.step(1.0 / 60.0, ex, &|_, _| 1.0);
        }
        assert_eq!(l.order().next().unwrap().key, 1);
    }
}

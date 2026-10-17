//! Network & System page (page 3, key `3`): per-link download / upload speeds with a
//! rate-over-time chart, connections and listening ports, containers and services.
//! Panels for things this machine doesn't have (containers, systemd) don't appear.
//!
//! "Right now" values are horizontal bars; "over time" charts are vertical columns
//! with a time axis ("2m ago … now"), a y axis in round units and a legend.

use super::*;
use crate::anim::{Val, W_LOAD};
use crate::net::*;

/// Network-page panels (`w == 0`: not shown).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct NetLay {
    pub net: Rect,
    pub conn: Rect,
    pub ctr: Rect,
    pub svc: Rect,
    /// what the layout was made for (containers, services, links up / down, virtual)
    sig: u64,
}

impl NetLay {
    pub fn rects(&self) -> [Rect; 4] {
        [self.net, self.conn, self.ctr, self.svc]
    }
}

/// An animated list row: values (`v[0..4]`) and its position (`v[4]`), which glides
/// when the order changes.
#[derive(Clone, Copy, Default)]
pub struct Item {
    pub(super) key: u64,
    idx: usize,
    pub(super) v: [Val; 5],
}

#[derive(Default)]
pub struct List {
    pub(super) it: Vec<Item>,
}

impl List {
    /// New data (key, values, sort key), sorted biggest first. The order shown changes
    /// only when an item clearly overtakes the one above it (by `hy` × + `abs`).
    pub(super) fn set(&mut self, new: &[(u64, [f32; 4], f32)], smooth: bool, hy: f32, abs: f32) {
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

    /// Smooth mode: one frame. Values show at `bsize`'s resolution (counts: whole),
    /// the first in tenths of a percent with `pct`.
    pub(super) fn step(&mut self, dt: f32, ex: f32, pct: bool) -> bool {
        let mut mv = false;
        for i in &mut self.it {
            for q in 0..4 {
                let u = if q == 0 && pct { 0.1 } else { bstep(i.v[q].x) };
                mv |= i.v[q].step(dt, W_LOAD, ex, u * 0.25);
                mv |= i.v[q].digits(dt, u, 0.0);
            }
            mv |= i.v[4].step(dt, W_LOAD, ex, 0.01);
        }
        mv
    }

    /// Rows in display order (by gliding position).
    pub(super) fn order(&self) -> Vec<usize> {
        let mut o: Vec<usize> = (0..self.it.len()).collect();
        crate::sys::isort(&mut o, &|&a, &b| self.it[a].v[4].x < self.it[b].v[4].x);
        o
    }

    pub(super) fn key(&self, bars: usize, top: f32) -> u64 {
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
    /// everything the logged-in users run (user.slice), one row
    users: List,
    peers: List,
}

const N_TEXT: usize = 0;
const N_CHART: usize = 1;
const N_CONN: usize = 2;
const N_CTR: usize = 3;
const N_SVC: usize = 4;
const N_TITLE: usize = 5;

/// A rate in decimal units (memui's `bsize` + "/s"): "4.71 kB/s", "12.3 MB/s".
pub fn nrate(v: f32) -> F {
    let mut f = bsize(v, false);
    f.str("/s");
    f
}

/// Short, for tight table cells: "1.61MB", "512B".
pub fn nshort(v: f32) -> F {
    let mut f = bsize(v, true);
    if f.n > 0 && f.s().as_bytes()[f.n - 1] != b'B' {
        f.push(b'B');
    }
    f
}

/// A round axis value ("10 MB/s", "1.5 kB/s"): whole numbers without decimals.
pub fn dtick(v: f32) -> F {
    // bsize's "10.0 MB" / "1.50 kB" without the trailing zeros
    let b = bsize(v, false);
    let (num, unit) = b.s().split_once(' ').unwrap_or((b.s(), ""));
    let num = if num.contains('.') {
        num.trim_end_matches('0').trim_end_matches('.')
    } else {
        num
    };
    fc(&[num, " ", unit, "/s"])
}

/// Chart scale for network rates: memui's 1, 2, 3, 5 × 10ⁿ steps, but from 10 kB/s
/// (a link is rarely as busy as a disk; quiet stays a flat line all the same).
pub fn net_scale(v: f32) -> f32 {
    nice_rate(v * 1e3) / 1e3
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

/// Below this (both ways together) a link counts as quiet (bytes/s).
pub(super) const QUIET_BPS: f32 = 10e3;

impl App {
    // ── layout ─────────────────────────────────────────────────────────────

    /// What the layout depends on: changes trigger a new layout.
    fn net_sig(&self) -> u64 {
        let n = &self.n;
        let (np, up, _) = n.links_up();
        mixk(&[
            n.ctrs.len().min(64) as u64,
            n.svcs.len().min(64) as u64,
            up as u64,
            np as u64,
            n.virt.is_empty() as u64,
        ])
    }

    /// Lay out again if the things shown changed (containers came or went...).
    pub fn net_refit(&mut self) -> bool {
        if self.page == PG_NET && self.nl.sig != self.net_sig() {
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
        let (np, nup, _) = n.links_up();
        let nup = nup.max(1);
        let extra = (np > nup) as usize + !n.virt.is_empty() as usize;
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
            if side {
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

    // ── animation ──────────────────────────────────────────────────────────

    /// New targets after a sample (`smooth`: values glide; else they jump).
    pub fn net_retarget(&mut self, smooth: bool) {
        if self.page != PG_NET && self.page != PG_OVERVIEW {
            return;
        }
        let (_, gw, b) = self.nchart_geo(self.nl.net.inner());
        let span = gw * b;
        let n = &self.n;
        let mut links: Vec<(u64, [f32; 4], f32)> = Vec::new();
        for i in n.ifs.iter().filter(|i| i.prim) {
            // one scale for the stacked chart: its highest column in view
            let mut m = i.rx + i.tx;
            if let Some(h) = i.h.as_ref() {
                for j in 0..h[0].len().min(span) {
                    m = m.max(h[0].back(j) + h[1].back(j));
                }
            }
            links.push((
                hkey(i.name.as_bytes()),
                [i.rx, i.tx, net_scale(m), 0.0],
                0.0,
            ));
        }
        let a = &mut self.nv;
        a.links.set(&links, smooth, f32::INFINITY, 0.0);
        if self.page == PG_OVERVIEW {
            // (the overview shows the links only)
            return;
        }
        a.estab.set(n.estab as f32, smooth);
        a.tw.set(n.tw as f32, smooth);
        let mut ctrs: Vec<(u64, [f32; 4], f32)> = Vec::new();
        for c in &n.ctrs {
            if c.ok {
                ctrs.push((c.k, [c.cpu, c.mem, c.rx, c.tx], c.cpu));
            }
        }
        a.ctrs.set(&ctrs, smooth, 1.2, 1.0);
        // services: the busiest first, then the biggest (100 MB counts like 1 % CPU)
        let mut sv: Vec<(u64, [f32; 4], f32)> = Vec::new();
        for s in &n.svcs {
            sv.push((s.k, [s.cpu, s.mem, 0.0, 0.0], s.cpu + s.mem / 1e8));
        }
        a.svc_cpu.set(&sv, smooth, 1.2, 0.5);
        let u: Vec<(u64, [f32; 4], f32)> = n
            .user
            .iter()
            .map(|s| (s.k, [s.cpu, s.mem, 0.0, 0.0], 0.0))
            .collect();
        a.users.set(&u, smooth, 1.0, 0.0);
        let mut peers: Vec<(u64, [f32; 4], f32)> = Vec::new();
        for p in &n.peers {
            peers.push((peer_key(p), [p.n as f32, 0.0, 0.0, 0.0], p.n as f32));
        }
        a.peers.set(&peers, smooth, 1.25, 1.0);
    }

    /// Smooth mode: one frame of every network-page spring and digit.
    pub fn net_step(&mut self, dt: f32) -> bool {
        if self.page != PG_NET && self.page != PG_OVERVIEW {
            return false;
        }
        let ex = (-W_LOAD * dt).exp();
        let a = &mut self.nv;
        let mut mv = a.links.step(dt, ex, false);
        mv |= a.ctrs.step(dt, ex, true);
        mv |= a.svc_cpu.step(dt, ex, true);
        mv |= a.users.step(dt, ex, true);
        mv |= a.peers.step(dt, ex, false);
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
        if let Some(i) = n.links_up().2.map(|k| &n.ifs[k]) {
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
    pub(crate) fn row(scr: &mut Screen, mut x: usize, y: usize, maxx: usize, cells: &[(&str, u32, i32)]) {
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

    pub(crate) fn blank(scr: &mut Screen, x: usize, y: &mut usize, end: usize, maxx: usize) {
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
    }

    /// Shown rate values of a link (smooth: gliding), chart scales.
    pub(super) fn link_vals(&self, name: &str) -> [Val; 4] {
        let k = hkey(name.as_bytes());
        self.nv
            .links
            .it
            .iter()
            .find(|i| i.key == k)
            .map_or([Val::default(); 4], |i| [i.v[0], i.v[1], i.v[2], i.v[3]])
    }

    /// The link the chart follows: the first that is up (default route first), else
    /// the first one there is.
    pub(super) fn main_link(&self) -> Option<usize> {
        let n = &self.n;
        (0..n.ifs.len())
            .find(|&i| n.ifs[i].prim && n.ifs[i].up)
            .or(n.ifs.iter().position(|i| i.prim))
    }

    /// Chart geometry inside the network panel's inner rect, like the disk chart's:
    /// (x of the first column, columns, seconds a column) — a 10-cell y-axis gutter,
    /// the values now at the right edge when there is room, 2–5 s a column for 2–5
    /// minutes.
    fn nchart_geo(&self, ir: Rect) -> (usize, usize, usize) {
        let gut = 10;
        let rg = if ir.w >= 48 { 13 } else { 0 };
        let gw = ir.w.saturating_sub(gut + rg).max(1);
        let b = (gw * 2).clamp(120, 300).div_ceil(gw).clamp(2, 5);
        (ir.x + gut, gw, b)
    }

    /// Column `c` of link `m`'s traffic: 0 is now (the rates as shown), then mean
    /// download / upload over whole `b`-second buckets of the history, aligned to the
    /// history count, so the columns step left once per bucket and never change in
    /// between.
    pub(super) fn link_col(&self, m: usize, c: usize, b: usize) -> Option<(f32, f32)> {
        let f = &self.n.ifs[m];
        if c == 0 {
            let v = self.link_vals(&f.name);
            return Some((v[0].n, v[1].n));
        }
        let h = f.h.as_ref()?;
        let lag = (self.n.pts % b as u64) as usize;
        let (lo, hi) = (lag + (c - 1) * b, lag + c * b);
        if hi > h[0].len() {
            return None;
        }
        let (mut d, mut u) = (0.0, 0.0);
        for p in lo..hi {
            d += h[0].back(p);
            u += h[1].back(p);
        }
        Some((d / b as f32, u / b as f32))
    }

    fn draw_links(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let now = crate::term::mono();
        let n = &self.n;
        let (mut ups, mut downs) = (Vec::new(), Vec::new());
        for (k, i) in n.ifs.iter().enumerate() {
            if i.prim {
                if i.up { &mut ups } else { &mut downs }.push(k);
            }
        }
        let main = self.main_link();
        // rows: a rate line per link up; then (keeping 6 rows for the chart) details,
        // links down, virtual links, a gap; the chart (caption, legend, columns) gets
        // the rest
        let mut rem = ih.saturating_sub(ups.len().max(1));
        let res = if rem >= 6 && main.is_some() && iw >= 30 {
            6
        } else {
            0
        };
        let det = !ups.is_empty() && rem >= res + ups.len();
        if det {
            rem -= ups.len();
        }
        let dl = !downs.is_empty() && rem > res;
        rem -= dl as usize;
        let vl = !n.virt.is_empty() && rem > res;
        rem -= vl as usize;
        let gap = res > 0 && rem >= 8;
        let ch = if res > 0 { rem - gap as usize } else { 0 };
        let y_chart = ir.y + ih - ch;
        let (cd, cu) = (self.t.accent, self.t.accent2);
        // the headline: what the network is doing now
        let (mut dn, mut up, mut bad) = (0.0f32, 0.0f32, false);
        for &i in &ups {
            let v = self.link_vals(&n.ifs[i].name);
            dn += v[0].n;
            up += v[1].n;
            bad |= n.ifs[i].bad(now);
        }
        let tk = mixk(&[
            dn.to_bits() as u64,
            up.to_bits() as u64,
            bad as u64,
            ups.len() as u64,
            iw as u64,
        ]);
        if self.nmemo(N_TITLE, tk) || full {
            let th = &self.t;
            let (d, u) = (nrate(dn), nrate(up));
            let lab = ("network", th.title | BOLD);
            let (dw, uw) = ((" download", th.dim), (" upload", th.dim));
            let warn = (" · packets lost", th.warn);
            let vs: [&[(&str, u32)]; 2] = [
                &[
                    lab,
                    (" ↓ ", cd),
                    (d.s(), cd | BOLD),
                    dw,
                    (" · ↑ ", cu),
                    (u.s(), cu | BOLD),
                    uw,
                    if bad { warn } else { ("", 0) },
                ],
                &[
                    lab,
                    (" ↓ ", cd),
                    (d.s(), cd | BOLD),
                    (" ↑ ", cu),
                    (u.s(), cu | BOLD),
                ],
            ];
            let one: [(&str, u32); 2] = if ups.is_empty() {
                [lab, (" not connected", th.crit | BOLD)]
            } else {
                [lab, (" quiet", th.good)]
            };
            let title = if ups.is_empty() || (dn + up < QUIET_BPS && !bad) {
                &one
            } else {
                Self::pick(r.w, &vs)
            };
            if full {
                self.panel(scr, r, title, "");
                if ch > 0 {
                    let (gx, gw, b) = self.nchart_geo(ir);
                    self.time_axis(scr, r, gx, gw, b as f32, "");
                }
            } else {
                self.panel_top(scr, r, title);
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let n = &self.n;
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
        if self.nmemo(N_TEXT, k) || full {
            let th = &self.t;
            let n = &self.n;
            let b = th.fg | BOLD;
            let mut y = ir.y;
            if ups.is_empty() {
                Self::row(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    &[("no network link is connected", th.crit, 0)],
                );
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
                let (d, u) = (nrate(v[0].n), nrate(v[1].n));
                let kd = kind(f);
                let kw = (nw + 2 - f.name.chars().count()) as i32;
                Self::row(
                    scr,
                    ir.x,
                    y,
                    maxx,
                    &[
                        (&f.name, b, 0),
                        (
                            if wide { kd.s() } else { "" },
                            th.mid,
                            if wide { kw } else { 0 },
                        ),
                        (if wide { "↓" } else { " ↓" }, cd, 0),
                        (d.s(), b, -10),
                        (if wide { " download   " } else { " " }, th.dim, 0),
                        ("↑", cu, 0),
                        (u.s(), b, -10),
                        (if wide { " upload" } else { "" }, th.dim, 0),
                    ],
                );
                y += 1;
                if det {
                    // speed, Wi-Fi signal, address, totals, then packets lost (dropped
                    // or broken) if it fits: amber while losses grow or the signal is weak
                    let mut d = F::new();
                    if f.speed > 0 {
                        d.str(link_speed(f.speed).s());
                        d.str(if iw >= 62 { " link · " } else { " · " });
                    }
                    let weak = f.kind == K_WIFI && f.sig != 0 && f.sig < -70;
                    if f.kind == K_WIFI && f.sig != 0 {
                        d.str("signal −");
                        d.int(f.sig.unsigned_abs(), 0);
                        d.str(if weak { " dBm (weak) · " } else { " dBm · " });
                    }
                    if !f.addr.is_empty() {
                        d.str(&f.addr);
                        d.str(" · ");
                    }
                    d.str("since boot ↓");
                    d.str(bsize(f.cnt[0] as f32, false).s());
                    d.str(" ↑");
                    d.str(bsize(f.cnt[1] as f32, false).s());
                    let lost = f.cnt[2] + f.cnt[3];
                    let p = fc(&[
                        " · ",
                        bsize(lost as f32, true).s().trim_end_matches('B'),
                        " packets lost",
                    ]);
                    if lost > 0 && 1 + d.s().chars().count() + p.s().chars().count() <= iw {
                        d.str(p.s());
                    }
                    let c = if f.bad(now) || weak { th.warn } else { th.mid };
                    Self::row(scr, ir.x, y, maxx, &[(" ", 0, 0), (d.s(), c, 0)]);
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
            // the chart's caption and legend
            if let (true, Some(m)) = (ch > 0, main) {
                let (_, gw, b) = self.nchart_geo(ir);
                let span = dur(gw * b);
                let bs = num("", b as f32, 0, " s");
                let name = &n.ifs[m].name;
                let cap = fc(&[
                    name,
                    " traffic, last ",
                    span.s(),
                    " · each column = ",
                    bs.s(),
                    " average",
                ]);
                Self::row(scr, ir.x, y_chart, maxx, &[(cap.s(), th.mid, 0)]);
                Self::row(
                    scr,
                    ir.x,
                    y_chart + 1,
                    maxx,
                    &[
                        ("■ ", cd, 0),
                        ("download (received)  ", th.text, 0),
                        ("■ ", cu, 0),
                        ("upload (sent), stacked", th.text, 0),
                    ],
                );
            }
        }
        if ch < 3 || !(graphs || full) {
            return;
        }
        let Some(m) = main else { return };
        let (gx, gw, b) = self.nchart_geo(ir);
        let v = self.link_vals(&self.n.ifs[m].name);
        let ck = mixk(&[
            self.n.pts / b as u64,
            iw as u64,
            ih as u64,
            v[2].x.to_bits() as u64,
            v[0].n.to_bits() as u64,
            v[1].n.to_bits() as u64,
        ]);
        if self.nmemo(N_CHART, ck) || full {
            self.link_chart(
                scr,
                ir,
                m,
                y_chart + 2,
                ch - 2,
                (gx, gw, b),
                v[2].x.max(1.0),
                full,
            );
        }
    }

    /// The traffic chart: stacked columns (download below, upload on top) on a round
    /// scale, newest at the right, the y axis on the left and the rates now at the
    /// right edge — the disk chart's grammar.
    #[allow(clippy::too_many_arguments)]
    fn link_chart(
        &self,
        scr: &mut Screen,
        ir: Rect,
        m: usize,
        cy: usize,
        ch: usize,
        geo: (usize, usize, usize),
        sc: f32,
        full: bool,
    ) {
        let (gx, gw, b) = geo;
        let th = &self.t;
        let bg = surf(S_PANEL);
        let maxx = ir.x + ir.w;
        let (cd, cu) = (th.accent, th.accent2);
        // y-axis: the scale, half of it, 0
        let (lt, lh) = (dtick(sc), dtick(sc / 2.0));
        let half = if ch >= 5 { ch / 2 } else { usize::MAX };
        for r in 0..ch {
            let lab = if r == 0 {
                lt.s()
            } else if r == half {
                lh.s()
            } else if r + 1 == ch {
                "0"
            } else {
                ""
            };
            Self::row(
                scr,
                ir.x,
                cy + r,
                gx - 1,
                &[(lab, th.dim, 1 - (gx - ir.x - 1) as i32)],
            );
            scr.put(
                gx - 1,
                cy + r,
                if lab.is_empty() { '│' } else { '┤' },
                th.border,
                bg,
            );
        }
        let e8 = (ch * 8) as f32;
        let mut cols = [(0, 0, false); 512];
        let n = gw.min(512);
        for (c, slot) in cols.iter_mut().enumerate().take(n) {
            let col = self.link_col(m, c, b);
            let (d, u) = col.unwrap_or((0.0, 0.0));
            let tot = d + u;
            let t8 = if col.is_some() && tot > 0.0 {
                ((tot / sc).min(1.0) * e8).round().max(1.0) as usize
            } else {
                0
            };
            let d8 = if tot > 0.0 {
                (t8 as f32 * d / tot).round() as usize
            } else {
                0
            };
            *slot = (t8 as u16, d8 as u16, col.is_some());
        }
        self.col_chart(scr, gx + gw - n, cy, ch, &cols[..n], (cd, cu), half, full);
        // the rates now at the right edge
        if gx + gw < maxx {
            let v = self.link_vals(&self.n.ifs[m].name);
            let (d, u) = (
                fc(&["↓ ", nrate(v[0].n).s()]),
                fc(&["↑ ", nrate(v[1].n).s()]),
            );
            for row in 0..ch {
                let sp: [(&str, u32, i32); 2] = match row {
                    0 => [(" now", th.mid, 0), ("", 0, 0)],
                    1 => [(" ", 0, 0), (d.s(), cd | BOLD, 0)],
                    2 => [(" ", 0, 0), (u.s(), cu | BOLD, 0)],
                    _ => [("", 0, 0), ("", 0, 0)],
                };
                Self::row(scr, gx + gw, cy + row, maxx, &sp);
            }
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
        let (w1, w2, w3) = if iw >= 50 {
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
                    let more = num("+ ", (nl - i) as f32, 0, " more");
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
            Self::row(scr, ir.x, y + 1, maxx, &[("most connections with", d, 0)]);
            y += 2;
            for k in a.peers.order() {
                let it = &a.peers.it[k];
                if y >= end {
                    break;
                }
                let Some(p) = n.peers.iter().find(|p| peer_key(p) == it.key) else {
                    continue;
                };
                let ip = fmt_ip(&p.ip, p.v6);
                let ps = num(":", p.port as f32, 0, "");
                let host = fc(&[if p.local { "this computer" } else { &ip }, ps.s(), " "]);
                // (a container's address, else a local port's owner: its container
                // when it publishes the port)
                let owner = n
                    .listen
                    .iter()
                    .find(|l| p.local && l.port == p.port && l.owner != "?")
                    .map(|l| l.owner.as_str());
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
        for (row, k) in list.order().into_iter().enumerate() {
            let it = &list.it[k];
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
                fc(&["↓", nshort(v[2].n).s()]),
                fc(&[" ↑", nshort(v[3].n).s()]),
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
        // the headline: anything unhealthy or restarting (red / amber), else all fine
        let (mut bad, mut rest, mut checked) = (0, 0, 0);
        for c in &n.ctrs {
            bad += (c.health == 3) as u32;
            checked += (c.health > 0) as u32;
            rest += (c.state == 1) as u32;
        }
        for c in &n.api {
            rest += (c.state == 1 && !n.ctrs.iter().any(|x| x.id == c.id)) as u32;
        }
        let k = mixk(&[
            iw as u64,
            ih as u64,
            self.nv.ctrs.key(12, 100.0),
            n.stopped as u64,
            (bad * 1000 + rest) as u64,
            n.api_err as u64,
        ]);
        if !(self.nmemo(N_CTR, k) || full) {
            return;
        }
        let (n, th) = (&self.n, &self.t);
        let nr = n.ctrs.len();
        let c = num(" ", nr as f32, 0, " running");
        let s = num(" · ", n.stopped as f32, 0, " stopped");
        let (hb, hr) = (
            num(" · ", bad as f32, 0, " unhealthy"),
            num(" · ", rest as f32, 0, " restarting"),
        );
        let state = if bad > 0 {
            (hb.s(), th.crit | BOLD)
        } else if rest > 0 {
            (hr.s(), th.warn | BOLD)
        } else if checked > 0 {
            (" · all healthy", th.good)
        } else {
            ("", 0)
        };
        let title: [(&str, u32); 4] = [
            ("containers", th.title | BOLD),
            (c.s(), th.text),
            state,
            (if n.stopped > 0 { s.s() } else { "" }, th.dim),
        ];
        if full {
            let sub = if n.api_err {
                "names: no access to docker"
            } else {
                "cpu: 100% = one core"
            };
            self.panel(scr, r, &title, sub);
        } else {
            self.panel_top(scr, r, &title);
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let maxx = ir.x + iw;
        let end = ir.y + ih;
        let find = |k: u64| n.ctrs.iter().find(|c| c.k == k);
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
        for c in &n.api {
            if c.state != 1 || n.ctrs.iter().any(|x| x.id == c.id) {
                continue;
            }
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
        // the services, busiest first, then the biggest
        let mut y = ir.y;
        if !a.users.it.is_empty() && ih >= 6 {
            y = self.usage_rows(scr, ir, y, y + 1, &a.users, "", false, false, &get);
            Self::row(scr, ir.x, y, maxx, &[]);
            y += 1;
        }
        let mut y = self.usage_rows(
            scr,
            ir,
            y,
            end,
            &a.svc_cpu,
            "services, busiest first",
            false,
            false,
            &get,
        );
        Self::blank(scr, ir.x, &mut y, end, maxx);
    }
}

/// Concatenated (cheaper in code than formatting).
pub(crate) fn fc(p: &[&str]) -> F {
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
        assert_eq!(nrate(12.34e6).s(), "12.3 MB/s");
        assert_eq!(nrate(512.0).s(), "512 B/s");
        assert_eq!(nshort(1.61e6).s(), "1.61MB");
        assert_eq!(nshort(0.0).s(), "0B");
        assert_eq!(dtick(10e6).s(), "10 MB/s");
        assert_eq!(dtick(1.5e3).s(), "1.5 kB/s");
        assert_eq!(net_scale(0.0), 10e3);
        assert_eq!(net_scale(2.6e6), 3e6);
        assert_eq!(net_scale(3.1e6), 5e6);
        assert_eq!(link_speed(1000).s(), "1 Gbit/s");
        assert_eq!(link_speed(2500).s(), "2.5 Gbit/s");
        assert_eq!(link_speed(100).s(), "100 Mbit/s");
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
                    a.page = PG_NET;
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
        a.page = PG_NET;
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
        assert_eq!(l.it[l.order()[0]].key, 1);
        // close: no flip-flop
        l.set(&d(10.0, 11.0), false, 1.2, 1.0);
        assert_eq!(l.it[l.order()[0]].key, 1);
        l.set(&d(10.0, 20.0), false, 1.2, 1.0);
        assert_eq!(l.it[l.order()[0]].key, 2);
        // smooth: positions glide to the new order
        l.set(&d(30.0, 20.0), true, 1.2, 1.0);
        assert_eq!(l.it[l.order()[0]].key, 2);
        let ex = (-W_LOAD / 60.0f32).exp();
        for _ in 0..60 {
            l.step(1.0 / 60.0, ex, false);
        }
        assert_eq!(l.it[l.order()[0]].key, 1);
    }
}

//! View 0: the overview — every page's answer on one screen (`0`, the default).
//!
//! One panel per page, each opening with that page's headline in its title, coloured
//! by health, then its key rows and a short history: CPU (load, clocks, watts,
//! temperature, every thread as a mini bar, load over time; on big screens every
//! core's load over time too), RAM (in use, swap, over time), disks (rates, busy, the
//! fullest filesystem, activity over time), network (the main link, containers'
//! health, traffic over time), the busiest and biggest programs, and sensors (the
//! hottest part, the CPU fan, power). Above them a strip says what needs attention,
//! from the pages' own health rules, or "all normal". Clicking a panel (or pressing
//! its number) opens its page. Small screens get one line per page.
//!
//! Sampling (main.rs): the pages' own samplers on cheap cadences — no per-process
//! memory scan, no sockets, no service statistics, no per-core power, a dear sensor
//! chip only every 10 minutes in eco; processes from one scan every 90 s (eco) with
//! the busiest followed every 6 s.

use super::*;
use crate::anim::W_LOAD;
use crate::color::Ramp;
use crate::f;
use crate::procs::Procs;
use crate::sensors::{K_FAN, K_TEMP, P_LOCKED, P_OK, R_BAT, R_GPU, R_PKG, R_PSYS};

/// Seconds a column of every overview chart (CPU, RAM, disk, network): the same
/// time scale side by side, so "1 min ago" is in the same place on each.
const OV_SECS: usize = 2;

/// Overview panels (`w == 0`: not shown).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct OvLay {
    /// what needs attention (one row)
    pub strip: Rect,
    pub cpu: Rect,
    /// every core's load over time (big screens)
    pub heat: Rect,
    pub mem: Rect,
    pub disk: Rect,
    pub net: Rect,
    pub procs: Rect,
    pub sens: Rect,
    /// small screens: a line per page (`cpu` holds its lines, bars and chart)
    pub lines: bool,
    sig: u64,
}

impl OvLay {
    pub fn rects(&self) -> [Rect; 7] {
        [self.cpu, self.heat, self.mem, self.disk, self.net, self.procs, self.sens]
    }
}

/// The page each of `OvLay::rects` opens.
const OV_PAGE: [usize; 7] = [PG_CPU, PG_CPU, PG_MEM, PG_MEM, PG_NET, PG_PROC, PG_SENS];

/// Overview state: layout, memo, the process scan and its animated lists, alerts.
#[derive(Default)]
pub struct OV {
    pub lay: OvLay,
    memo: Vec<u64>,
    /// processes (the overview's own scan; dropped when the page is left)
    pub d: Option<Procs>,
    last_scan: i64,
    /// programs by CPU and by memory: (key, values [cpu %, memory], sort key)
    busy: List,
    big: List,
    /// program names by key
    names: Vec<(u64, String)>,
    /// where the strip's alerts are (x from, x to, page)
    zones: Vec<(usize, usize, usize)>,
    /// CPU time (ns) of the dear jobs (process scans, container look-ups) since the
    /// main loop last took it: planned for, outside the per-sample estimate
    pub spent: i64,
    /// a container look-up's and a read of the links' cost (slowly decaying peaks, ns)
    job: i64,
    dev: i64,
    /// the memory page's process scan's cost (a slowly decaying peak, ns)
    mscan: i64,
    /// the threads' mini bars: (eighths shown, frame of the last change, direction)
    lv: Vec<(u16, u64, i8)>,
    /// the rate charts' newest column as shown (disk, network: eighths of a cell,
    /// of them the lower part), the frame it changed and in which direction
    head: [std::cell::Cell<(u16, u16, u64, i8)>; 2],
    /// their scales and when they changed (smooth: a scale shrinks a second after)
    scale: [std::cell::Cell<(f32, u64, u64)>; 2],
    /// the hottest part shown last (see `ov_hottest`)
    hot: std::cell::Cell<usize>,
}

// memo slots
const O_STRIP: usize = 0;
const O_CPU_T: usize = 1;
const O_CPU_R: usize = 2;
const O_CPU_B: usize = 3;
const O_CPU_G: usize = 4;
const O_HEAT: usize = 5;
const O_MEM_T: usize = 6;
const O_MEM_R: usize = 7;
const O_MEM_G: usize = 8;
const O_DISK_T: usize = 9;
const O_DISK_R: usize = 10;
const O_DISK_G: usize = 11;
const O_NET_T: usize = 12;
const O_NET_R: usize = 13;
const O_NET_G: usize = 14;
const O_PROC: usize = 15;
const O_SENS: usize = 16;
const O_LINES: usize = 17;

/// Programs listed at most.
const OV_NPROG: usize = 12;
/// Processes followed between scans (their CPU % every few seconds).
const OV_TRACK: usize = 6;

/// A temperature's plain name from the CPU sampler's label ("Tccd1" → "CPU chiplet 1").
fn cpu_tname(l: &str) -> F {
    match l.strip_prefix("Tccd") {
        Some(n) => f!("CPU chiplet {n}"),
        None => f!("CPU"),
    }
}

/// A share of the machine's CPU: "0%", "0.4%", "12%".
fn cpct(c: f32) -> F {
    if c < 0.05 {
        f!("0%")
    } else if c < 9.95 {
        pct1(c / 100.0)
    } else {
        num("", c, 0, "%")
    }
}

/// "68 °C" (a space: easier to read in a sentence than the pages' "68°C").
fn degc(v: f32) -> F {
    num("", v, 0, " °C")
}

impl App {
    fn omemo(&mut self, slot: usize, key: u64) -> bool {
        let m = &mut self.ov.memo;
        if m.len() <= slot {
            m.resize(slot + 1, u64::MAX);
        }
        let ch = m[slot] != key;
        m[slot] = key;
        ch
    }

    /// A row: label → value → bar (as wide as the note leaves room for, up to 24
    /// cells; no bar when `ramp` is None) → note.
    #[allow(clippy::too_many_arguments)]
    fn orow(&self, scr: &mut Screen, x: usize, y: usize, maxx: usize, lab: &str, val: (&str, u32), bar: Option<(f32, &Ramp, usize)>, note: (&str, u32)) {
        let (f, ramp, bw) = bar.unwrap_or((0.0, &self.t.load, 0));
        // (a note that doesn't fit whole is left out)
        let fits = x + LW + VW + 1 + bw + 1 + note.0.chars().count() <= maxx;
        self.srow(scr, x, y, maxx, (lab, self.t.text), val, (f, bw, ramp), ("", 0), &[(" ", 0), if fits { note } else { ("", 0) }]);
    }

    /// The bars' width in a panel `iw` wide whose rows have notes up to `nl` cells
    /// after them (4 to 24 cells).
    fn ov_bw(iw: usize, nl: usize) -> usize {
        iw.saturating_sub(LW + VW + 2 + nl).clamp(4, 24)
    }

    // ── data ───────────────────────────────────────────────────────────────

    /// The page came into view: everything it shows, fresh (input handling, outside
    /// the eco budget).
    pub fn ov_enter(&mut self) {
        let now = crate::term::mono();
        self.m.refresh_fs();
        let eco = SPEEDS[self.speed].1 == 0;
        let _ = self.n.tick_ov(now, 0, eco, true);
        let cpu_t = !self.s.temps.is_empty();
        self.z.tick_ov(now, 0, eco, cpu_t, true);
        let d = self.ov.d.get_or_insert_with(|| Procs::new("/proc"));
        d.statm = false;
        d.reset();
        d.scan(now, 0);
        d.release();
        self.ov.last_scan = now;
        self.ov_track(now);
        self.m.scan_procs();
        self.m.shrink();
        self.ov_retarget(false);
        self.net_retarget(false);
        self.sens_retarget(false);
    }

    /// The page was left: the process scan's memory back (the scanner stays, closed).
    pub fn ov_leave(&mut self) {
        if let Some(d) = self.ov.d.as_mut() {
            d.reset();
            d.release();
        }
        self.ov.busy = List::default();
        self.ov.big = List::default();
        self.ov.names = Vec::new();
    }

    /// Follow the busiest processes of the last scan (their `stat` kept open).
    fn ov_track(&mut self, now: i64) {
        let Some(d) = self.ov.d.as_mut() else { return };
        let mut v: Vec<(f32, u32, u64)> = d
            .list
            .iter()
            .filter(|p| p.gone == 0 && p.cpu > 0.0)
            .map(|p| (p.cpu, p.pid, p.start))
            .collect();
        crate::sys::isort(&mut v, &|a, b| a.0 > b.0);
        let want: Vec<(u32, u64)> = v.iter().take(OV_TRACK).map(|x| (x.1, x.2)).collect();
        d.track(&want);
        d.read_tracked(now, false);
    }

    /// One sample while the overview is shown (main loop): processes (a full scan
    /// every 90 s in eco, every 6 s otherwise; the busiest re-read every 6 s /
    /// 0.4 s).
    /// `quiet`: a dear sensor read is near (eco), scans wait unless long overdue.
    /// Containers (net::Net::tick_ov) are looked up here too.
    pub fn ov_sample(&mut self, now: i64, eco: bool, smooth: bool, push: u32, quiet: bool) {
        const SEC: i64 = 1_000_000_000;
        let (dv, c) = self.n.tick_ov(now, push, eco, false);
        let peak = |p: &mut i64, c: i64| *p = if c > *p { c } else { *p - (*p - c) / 8 };
        if dv > 1000 {
            peak(&mut self.ov.dev, dv);
        }
        if c > 1000 {
            peak(&mut self.ov.job, c);
        }
        self.ov.spent += dv + c;
        let Some(d) = self.ov.d.as_mut() else { return };
        let gap = Self::ov_gap(d, eco);
        let late = now - self.ov.last_scan;
        // (the first scan gives no rates: a second one soon)
        let scan = (late >= gap - SEC / 5 && !(quiet && late < 2 * gap)) || (d.scans == 1 && late >= 2 * SEC);
        let track = now - d.last_track >= if eco { 6 * SEC } else { 2 * SEC / 5 } - SEC / 20;
        if scan {
            let t0 = crate::term::cpu_ns();
            #[cfg(windows)]
            d.set_eco(eco);
            d.scan(now, 0);
            d.release();
            self.ov.last_scan = now;
            self.ov_track(now);
            self.ov.spent += crate::term::cpu_ns() - t0;
        } else if track {
            d.read_tracked(now, false);
        }
        // the biggest programs as the memory page counts them: its full scan every 3
        // minutes (eco; else 30 s), the biggest followed live every 20 s (4 s)
        let m = &mut self.m;
        let (gap, live) = if eco { (180, 20) } else { (30, 4) };
        let mscan = now - m.last_proc >= gap * SEC - SEC / 5 && !quiet;
        let mem = if mscan {
            let t0 = crate::term::cpu_ns();
            m.scan_procs();
            m.shrink();
            let c = crate::term::cpu_ns() - t0;
            self.ov.spent += c;
            self.ov.mscan = if c > self.ov.mscan { c } else { self.ov.mscan - (self.ov.mscan - c) / 8 };
            true
        } else if now - m.last_top >= live * SEC - SEC / 20 {
            m.last_top = now;
            m.refresh_procs();
            true
        } else {
            false
        };
        if scan || track || mem {
            self.ov_retarget(smooth);
        }
        // (a scan's scratch memory back to the system)
        #[cfg(target_env = "gnu")]
        if scan || mscan {
            unsafe { libc::malloc_trim(0) };
        }
    }

    /// Full process scans' spacing: eco 90 s (or longer if a scan is dear: at most
    /// ~1/90 000 of the time), else 6 s.
    fn ov_gap(d: &Procs, eco: bool) -> i64 {
        const SEC: i64 = 1_000_000_000;
        if eco {
            (90 * SEC).max(d.cost * 90_000)
        } else {
            (6 * SEC).max(d.cost * 4000)
        }
    }

    /// The CPU time (ns a second) planned for the dear jobs (eco): process scans, the
    /// links every 10 s, the container look-ups (every 5 and 2 minutes), the memory
    /// page's process scan (3 minutes).
    pub fn ov_planned(&self) -> i64 {
        const SEC: i64 = 1_000_000_000;
        let p = self.ov.d.as_ref().map_or(0, |d| d.cost.max(200_000) * SEC / Self::ov_gap(d, true));
        let j = self.ov.job * if self.n.ctrs.is_empty() { 1 } else { 4 } / 600;
        p + j + self.ov.dev / 10 + self.ov.mscan / 180
    }

    /// Programs (processes grouped by name, as the processes page groups them) by CPU
    /// and by memory: new targets.
    pub fn ov_retarget(&mut self, smooth: bool) {
        let Some(d) = self.ov.d.as_ref() else { return };
        // busiest: processes grouped by name (key, CPU, resident, processes, a member)
        type G = (u64, f32, f32, u32, usize);
        let mut g: Vec<G> = Vec::with_capacity(64);
        for (j, p) in d.list.iter().enumerate() {
            if p.gone != 0 {
                continue;
            }
            let k = name_key(p.group());
            match g.iter_mut().find(|x| x.0 == k) {
                Some(x) => {
                    x.1 += p.cpu;
                    x.2 += p.rss as f32;
                    x.3 += 1;
                }
                None => g.push((k, p.cpu, p.rss as f32, 1, j)),
            }
        }
        // biggest: the memory page's programs (private memory, shared parts once), so
        // the numbers agree with it and add up to the RAM in use
        let m = &self.m;
        let mem_of = |k: u64| m.top.iter().map(|&i| &m.groups[i]).find(|x| name_key(&x.name) == k).map(|x| x.mem as f32);
        // (CPU as a share of the whole machine, like the CPU panel's "busy")
        let share = 100.0 / self.s.f.len().max(1) as f32;
        let mut b: Vec<(u64, [f32; 4], f32)> = g.iter().map(|x| (x.0, [x.1 * share, mem_of(x.0).unwrap_or(x.2), x.3 as f32, 0.0], x.1 * share)).collect();
        crate::sys::isort(&mut b, &|a, b| a.2 > b.2);
        b.truncate(OV_NPROG);
        // (the process count from the same live list as the busiest, so a program
        // shows the same "×n" in both lists; the memory scan's count can be minutes old)
        let live_n = |k: u64, n: u32| g.iter().find(|x| x.0 == k).map_or(n as f32, |x| x.3 as f32);
        let mut big: Vec<(u64, [f32; 4], f32)> = m.top.iter().take(OV_NPROG).map(|&i| &m.groups[i]).map(|x| { let k = name_key(&x.name); (k, [0.0, x.mem as f32, live_n(k, x.n), 0.0], x.mem as f32) }).collect();
        if big.is_empty() {
            // (not scanned yet: resident sizes)
            big = g.iter().map(|x| (x.0, [0.0, x.2, x.3 as f32, 0.0], x.2)).collect();
            crate::sys::isort(&mut big, &|a, b| a.2 > b.2);
            big.truncate(OV_NPROG);
        }
        // (keep their order unless clearly overtaken: CPU by 10 % and 0.05 points,
        // memory 3 %)
        self.ov.busy.set(&b, smooth, 1.1, 0.05);
        self.ov.big.set(&big, smooth, 1.03, 2e6);
        // names of the programs listed (and of those leaving, while they fade)
        let mut names = std::mem::take(&mut self.ov.names);
        names.retain(|n| self.ov.busy.it.iter().chain(&self.ov.big.it).any(|i| i.key == n.0));
        for x in &g {
            if b.iter().any(|y| y.0 == x.0) && !names.iter().any(|n| n.0 == x.0) {
                names.push((x.0, d.list[x.4].group().to_string()));
            }
        }
        for &i in &m.top {
            let x = &m.groups[i];
            let k = name_key(&x.name);
            if big.iter().any(|y| y.0 == k) && !names.iter().any(|n| n.0 == k) {
                names.push((k, x.name.clone()));
            }
        }
        self.ov.names = names;
    }

    /// Smooth mode: one frame of the overview's own springs (programs).
    pub fn ov_step(&mut self, dt: f32) -> bool {
        if self.page != PG_OVERVIEW {
            return false;
        }
        let ex = (-W_LOAD * dt).exp();
        self.ov.busy.step(dt, ex, true) | self.ov.big.step(dt, ex, false)
    }

    // ── layout ─────────────────────────────────────────────────────────────

    /// What the layout depends on (what this machine has).
    fn ov_sig(&self) -> u64 {
        mixk(&[
            (self.m.disks.len() + self.m.fs.len()).min(1) as u64,
            self.n.ctrs.len().min(1) as u64,
            self.ov_sens_rows() as u64,
            self.ov_cpu_stats()[3] as u64,
            self.s.temps.is_empty() as u64,
        ])
    }

    /// Lay out again when what's shown changed (containers came, sensors found...).
    pub fn ov_refit(&mut self) -> bool {
        if self.page == PG_OVERVIEW && self.ov.lay.sig != self.ov_sig() {
            self.fit(self.w, self.h);
            return true;
        }
        false
    }

    /// CPU statistics rows the panel can show: busy, temperature, clock, power.
    fn ov_cpu_stats(&self) -> [bool; 4] {
        let z = &self.z;
        [
            true,
            !self.s.temps.is_empty(),
            self.s.t.has_freq,
            z.cpu_power == P_OK && z.role(R_PKG).is_some_and(|i| z.rd[i].ok),
        ]
    }

    /// Sensors panel rows: hottest, fan, power (0: nothing to show).
    fn ov_sens_rows(&self) -> usize {
        let z = &self.z;
        let t = !self.s.temps.is_empty() || z.rd.iter().any(|r| r.kind == K_TEMP && r.ok);
        let f = z.rd.iter().any(|r| r.kind == K_FAN && r.seen);
        let p = z.role(R_PKG).is_some() || z.role(R_GPU).is_some() || !z.bats.is_empty() || z.cpu_power == P_LOCKED;
        t as usize + f as usize + p as usize
    }

    /// Panels in `area`: the strip, then one, two or three columns of panels, or a
    /// line per page on small screens.
    pub fn ov_layout(&self, area: Rect) -> OvLay {
        let (w, h) = (area.w, area.h);
        let mut l = OvLay {
            sig: self.ov_sig(),
            ..Default::default()
        };
        if h == 0 {
            return l;
        }
        l.strip = Rect { h: 1, ..area };
        let r = Rect {
            y: area.y + 1,
            h: h - 1,
            ..area
        };
        let ncol = if w >= 190 && h >= 30 {
            3
        } else if w >= 76 && h >= 18 {
            2
        } else if w >= 50 && h >= 34 {
            1
        } else {
            l.lines = true;
            l.cpu = r;
            return l;
        };
        const BIG: usize = 1 << 20;
        let nst = self.ov_cpu_stats().iter().filter(|&&b| b).count();
        let has_disk = !self.m.disks.is_empty() || !self.m.fs.is_empty();
        let sr = self.ov_sens_rows();
        let ctr = !self.n.ctrs.is_empty() as usize;
        // (min, comfortable, want, max) outer heights; 0: absent
        let cpu = (3, 2 + 3 + nst.min(2), 2 + nst + 1 + 3 + 7, BIG);
        let n = self.ngroups();
        let hh = n.div_ceil(2) + 4;
        let heat = if ncol == 3 && n >= 4 { (hh, hh, hh, hh) } else { (0, 0, 0, 0) };
        let mem = (3, 4, 2 + 2 + 6, BIG);
        let disk = if has_disk { (3, 4, 2 + 2 + 6, BIG) } else { (0, 0, 0, 0) };
        let net = (3, 3 + ctr, 2 + 1 + ctr + 6, BIG);
        let procs = (4, 6, 2 + 1 + 5, 2 + 1 + OV_NPROG);
        let sens = if sr > 0 { (3, 2 + sr, 2 + sr, 2 + sr) } else { (0, 0, 0, 0) };
        // panels: 0 cpu, 1 heat, 2 mem, 3 disk, 4 net, 5 procs, 6 sens
        let need = [cpu, heat, mem, disk, net, procs, sens];
        let cols: &[&[usize]] = match ncol {
            3 => &[&[0, 1, 6], &[2, 3], &[4, 5]],
            2 => &[&[0, 2, 6], &[3, 4, 5]],
            _ => &[&[0, 2, 3, 4, 6, 5]],
        };
        let ws: [usize; 3] = match ncol {
            3 => [w * 36 / 100, w * 32 / 100, w - w * 36 / 100 - w * 32 / 100],
            2 => [w / 2, w - w / 2, 0],
            _ => [w, 0, 0],
        };
        let mut out = [Rect::default(); 7];
        let mut x = r.x;
        for (c, ps) in cols.iter().enumerate() {
            let want: Vec<(usize, usize, usize, usize)> = ps.iter().map(|&p| need[p]).collect();
            let hs = ostack(r.h, &want);
            let mut y = r.y;
            for (k, &p) in ps.iter().enumerate() {
                if hs[k] > 0 {
                    out[p] = Rect { x, y, w: ws[c], h: hs[k] };
                    y += hs[k];
                }
            }
            x += ws[c];
        }
        [l.cpu, l.heat, l.mem, l.disk, l.net, l.procs, l.sens] = out;
        l
    }

    /// The page at screen cell (x, y) (a panel, a line, an alert), if any.
    pub fn ov_at(&self, x: usize, y: usize) -> Option<usize> {
        if self.page != PG_OVERVIEW {
            return None;
        }
        let l = &self.ov.lay;
        if l.strip.contains(x, y) {
            return self.ov.zones.iter().find(|z| x >= z.0 && x < z.1).map(|z| z.2);
        }
        if l.lines {
            // (the line rows, see ov_lines)
            let rows = self.ov_line_rows();
            return rows.iter().find(|r| r.1.contains(x, y)).map(|r| r.0);
        }
        l.rects().iter().zip(OV_PAGE).find(|(r, _)| r.contains(x, y)).map(|(_, p)| p)
    }

    /// Title-bar summary: the CPU model and size, RAM.
    pub fn ov_info(&self) -> (F, F) {
        let s = &self.s.t;
        let a = f!("   {} · {} cores · {} threads", s.short_model, s.groups.len(), s.cpus.len());
        let b = fc(&[" · RAM ", bsize(self.m.mi.total as f32, false).s()]);
        (a, b)
    }

    // ── what needs attention ───────────────────────────────────────────────

    /// The hottest temperature: (plain name, °C shown, amber, red), from the CPU
    /// sampler's sensors and the sensors page's others (unlabelled inputs left out).
    fn ov_hottest(&self) -> Option<(F, f32, f32, f32)> {
        // (source: CPU sampler sensor k, or sensors-page reading usize::MAX / 2 + i;
        // by how close to its limit; the one shown last stays unless another is
        // clearly closer, so two sensors at the same heat don't take turns)
        let last = self.ov.hot.get();
        let mut best: Option<(usize, f32, f32, f32)> = None;
        let mut take = |src: usize, v: f32, w: f32, c: f32| {
            let m = |s: usize| if s == last { 0.015 } else { 0.0 };
            if best.is_none_or(|b| v / c + m(src) > b.1 / b.3 + m(b.0) + 1e-3) {
                best = Some((src, v, w, c));
            }
        };
        for k in 0..self.s.temps.len() {
            let v = self.temp(k).1;
            if v > 0.0 {
                take(k, v, 85.0, 95.0);
            }
        }
        let cpu_t = !self.s.temps.is_empty();
        const Z: usize = usize::MAX / 2;
        for (i, r) in self.z.rd.iter().enumerate() {
            if r.kind == K_TEMP && r.ok && !r.guess && !r.minor && self.z.ov_wants(r.src as usize, cpu_t) {
                take(Z + i, self.zv(i).n, r.warn, r.crit);
            }
        }
        // RAM sticks and drives (the memory page's helper thread reads them)
        const D: usize = usize::MAX / 4;
        let m = &self.m;
        for (k, &i) in m.dimms.iter().enumerate() {
            if let Some(t) = crate::mem::slow_temp(i).filter(|&t| t > 0.0) {
                take(D + k, t, DIMM_WARN, DIMM_CRIT);
            }
        }
        for (k, d) in m.disks.iter().enumerate() {
            if let Some(t) = d.temp.and_then(crate::mem::slow_temp).filter(|&t| t > 0.0) {
                take(D + 64 + k, t, DISK_WARN, DISK_CRIT);
            }
        }
        let (src, v, w, c) = best?;
        self.ov.hot.set(src);
        let name = if src >= Z {
            f!("{}", self.z.rd[src - Z].name)
        } else if src >= D + 64 {
            let n = &m.disks[src - D - 64].name;
            f!("{} ({})", if n.starts_with("nvme") { "SSD" } else { "drive" }, n)
        } else if src >= D {
            if m.dimms.len() > 1 { f!("RAM stick {}", src - D + 1) } else { f!("RAM") }
        } else {
            cpu_tname(&self.s.temps[src].label)
        };
        Some((name, v, w, c))
    }

    /// The CPU fan (or the first spinning one): (reading index, stopped).
    fn ov_fan(&self) -> Option<(usize, bool)> {
        let z = &self.z;
        let (on, _) = self.fans();
        let cpu = on.iter().copied().find(|&i| z.rd[i].name.starts_with("CPU fan"));
        let i = cpu.or(on.first().copied())?;
        let r = &z.rd[i];
        Some((i, cpu.is_some() && r.ok && r.val == 0.0 && r.max > 300.0))
    }

    /// What needs attention, worst first: (1 watch / 2 act, page, text).
    pub fn ov_alerts(&self) -> Vec<(u8, usize, F)> {
        let mut v: Vec<(u8, usize, F)> = Vec::new();
        // temperatures
        if let Some((n, t, w, c)) = self.ov_hottest() {
            let lv = level(t, w, c);
            if lv > 0 {
                let cpu = n.s().starts_with("CPU");
                let s = match (lv, cpu) {
                    (2, true) => fc(&[n.s(), " at ", degc(t).s(), " — limited by temperature"]),
                    (2, _) => fc(&[n.s(), " too hot: ", degc(t).s()]),
                    // (one word per level everywhere: "warm", then "too hot")
                    _ => fc(&[n.s(), " warm: ", degc(t).s()]),
                };
                v.push((lv, PG_SENS, s));
            }
        }
        if let Some((i, true)) = self.ov_fan() {
            v.push((2, PG_SENS, fc(&[&self.z.rd[i].name, " stopped"])));
        }
        // RAM, swap
        let total = (self.m.mi.total as f32).max(1.0);
        let a = &self.ma;
        let ul = level(a.used.n / total, RAM_WARN, RAM_CRIT);
        let lv = self.ram_level();
        let p = num("", a.used.n / total * 100.0, 0, "% in use");
        if ul > 0 {
            v.push((ul, PG_MEM, f!("RAM {}: {}", if ul > 1 { "nearly full" } else { "getting full" }, p.s())));
        } else if lv > 0 && a.psi.n >= PSI_WARN {
            v.push((lv, PG_MEM, fc(&["RAM is tight: programs wait for it ", num("", a.psi.n, 0, "%").s(), " of the time"])));
        }
        if self.m.mi.swap_total > 0 && a.swio.n >= SWAP_WARN {
            let sl = level(a.swio.n, SWAP_WARN, SWAP_CRIT).min(lv.max(1));
            v.push((sl, PG_MEM, fc(&["swap active: ", rate(a.swio.n, false).s()])));
        }
        // filesystems (the two fullest)
        let mut fs: Vec<(u32, usize)> = self.m.fs.iter().enumerate().map(|(i, f)| (f.pct(), i)).filter(|x| x.0 as f32 >= FS_WARN).collect();
        crate::sys::isort(&mut fs, &|a, b| a.0 > b.0);
        for &(p, i) in fs.iter().take(2) {
            let f = &self.m.fs[i];
            v.push((level(p as f32, FS_WARN, FS_CRIT), PG_MEM, f!("{} {}% full · {} free", f.mnt, p, bsize(f.avail as f32, false).s())));
        }
        // network, containers
        let n = &self.n;
        let (np, up, _) = n.links_up();
        let now = crate::term::mono();
        if np > 0 && up == 0 {
            v.push((2, PG_NET, f!("network not connected")));
        }
        if let Some(i) = n.ifs.iter().find(|i| i.prim && i.up && i.bad(now)) {
            v.push((1, PG_NET, fc(&[&i.name, " losing packets"])));
        }
        for c in &n.ctrs {
            if c.health == 3 {
                v.push((2, PG_NET, fc(&["container ", &c.name, " unhealthy"])));
            } else if c.state == 1 {
                v.push((1, PG_NET, fc(&["container ", &c.name, " restarting"])));
            }
        }
        // battery
        for b in &self.z.bats {
            if b.st == 2 && b.pct >= 0.0 && b.pct < 20.0 {
                v.push((1 + (b.pct < 10.0) as u8, PG_SENS, fc(&["battery ", num("", b.pct, 0, "%").s(), " and discharging"])));
            }
        }
        crate::sys::isort(&mut v, &|a, b| a.0 > b.0);
        v
    }

    // ── drawing ────────────────────────────────────────────────────────────

    /// The overview. `graphs`: also the charts (not on bars-only frames).
    pub fn draw_ov(&mut self, scr: &mut Screen, full: bool, graphs: bool) {
        if full {
            self.ov.memo.iter_mut().for_each(|c| *c = u64::MAX);
        }
        if self.zv.v.len() != self.z.rd.len() {
            self.sens_retarget(false);
        }
        let (a, m) = (&self.ma, &self.m);
        if a.disk.len() != m.disks.len() || a.fs.len() != m.fs.len() {
            self.mem_retarget(self.smooth());
        }
        let l = self.ov.lay;
        if graphs || full {
            prof(12, || self.ov_strip(scr, l.strip, full));
        }
        if l.lines {
            self.ov_lines(scr, full, graphs);
            return;
        }
        if l.cpu.w > 0 {
            prof(9, || self.ov_cpu(scr, l.cpu, full, graphs));
        }
        if !(graphs || full) {
            // (bars-only frames: the CPU's bars and numbers)
            return;
        }
        if l.heat.w > 0 {
            let k = mixk(&[self.body_key(2), self.selected as u64]);
            if self.omemo(O_HEAT, k) || full {
                self.draw_heat(scr, l.heat, full, false);
            }
        }
        if l.mem.w > 0 {
            prof(11, || self.ov_mem(scr, l.mem, full));
        }
        if l.disk.w > 0 {
            prof(11, || self.ov_disk(scr, l.disk, full));
        }
        if l.net.w > 0 {
            prof(11, || self.ov_net(scr, l.net, full));
        }
        if l.procs.w > 0 {
            prof(10, || self.ov_procs(scr, l.procs, full));
        }
        if l.sens.w > 0 {
            prof(10, || self.ov_sens(scr, l.sens, full));
        }
    }

    /// A panel's title with the page's key at the right ("2 ›": press or click).
    fn ov_title(&self, scr: &mut Screen, r: Rect, full: bool, vs: &[&[(&str, u32)]], page: usize) {
        let tag = r.w >= 30;
        let title = Self::pick(r.w.saturating_sub(if tag { 5 } else { 0 }), vs);
        if full {
            self.panel(scr, r, title, "");
        } else {
            self.panel_top(scr, r, title);
        }
        if tag {
            let d = [b'0' + page as u8];
            let d = std::str::from_utf8(&d).unwrap_or("");
            let x = r.x + r.w - 5;
            let bg = surf(S_PANEL);
            scr.put(x, r.y, ' ', 0, bg);
            scr.text(x + 1, r.y, d, self.t.accent | BOLD, bg, x + 2);
            scr.text(x + 2, r.y, " ›", self.t.dim, bg, x + 4);
        }
    }

    /// The strip: "all normal", or what needs attention (each opens its page).
    fn ov_strip(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        if r.w == 0 {
            return;
        }
        let al = self.ov_alerts();
        let mut k = mixk(&[r.w as u64, al.len() as u64]);
        for a in &al {
            k = mixk(&[k, a.0 as u64, name_key(a.2.s())]);
        }
        if !(self.omemo(O_STRIP, k) || full) {
            return;
        }
        let th = &self.t;
        let bg = surf(S_CHIP);
        let maxx = r.x + r.w;
        self.ov.zones.clear();
        let mut x = r.x;
        if al.is_empty() {
            let hint = if r.w >= 100 { " · nothing needs attention · click a panel or press its number for more" } else if r.w >= 60 { " · nothing needs attention" } else { "" };
            Self::line(scr, x, r.y, maxx, bg, &[(" ● ", th.good), ("all normal", th.good | BOLD), (hint, th.mid)]);
            return;
        }
        let worst = al[0].0;
        let c = |s: u8| if s > 1 { th.crit | BOLD } else { th.warn | BOLD };
        let head = if r.w >= 60 { if worst > 1 { " ▲ act now: " } else { " ▲ needs attention: " } } else { " ▲ " };
        x = Self::spans(scr, x, r.y, maxx, bg, &[(head, c(worst))]);
        for (k, a) in al.iter().enumerate() {
            let more = f!(" +{} more", al.len() - k);
            let n = a.2.s().chars().count() + if k > 0 { 3 } else { 0 };
            let room = if k + 1 < al.len() { more.n } else { 0 };
            if x + n + room > maxx && k > 0 {
                x = Self::spans(scr, x, r.y, maxx, bg, &[(more.s(), th.mid)]);
                break;
            }
            if k > 0 {
                x = Self::spans(scr, x, r.y, maxx, bg, &[(" · ", th.dim)]);
            }
            let x0 = x;
            x = Self::spans(scr, x, r.y, maxx, bg, &[(a.2.s(), c(a.0))]);
            self.ov.zones.push((x0, x, a.1));
        }
        Self::line(scr, x, r.y, maxx, bg, &[]);
    }

    /// Mini bars of every thread (one column each, grouped by core and CCD; one per
    /// core if threads don't fit) `hb` rows high at (x, y) in `w` cells, and the core
    /// numbers under them when `lab`. Returns the width used.
    #[allow(clippy::too_many_arguments)]
    fn ov_bars(&self, scr: &mut Screen, x: usize, y: usize, w: usize, hb: usize, lab: bool) -> usize {
        let s = &self.s;
        let t = &s.t;
        let n = t.groups.len();
        let nth: usize = t.groups.iter().map(|g| g.len()).sum();
        let _ = nth;
        let (per, cg, dg) = self.ov_bar_cols(w);
        let bg = surf(S_PANEL);
        let th = &self.t;
        let e = 8 * hb;
        let mut xx = x;
        let lvl = |c: usize| self.ov.lv.get(c).map_or(0, |l| l.0 as usize);
        let col = |scr: &mut Screen, xx: usize, lv: usize| {
            let c = th.load.at(lv as f32 / e as f32);
            for r in 0..hb {
                let (g, fg) = self.vbar_cell(c, lv, r, hb);
                scr.put(xx, y + hb - 1 - r, g, fg, bg);
            }
        };
        for g in 0..n {
            if xx >= x + w {
                break;
            }
            if g > 0 {
                let gap = if t.ccd[g] != t.ccd[g - 1] { dg } else { cg };
                for _ in 0..gap {
                    for r in 0..hb + lab as usize {
                        scr.put(xx, y + r, ' ', NOPATCH, bg);
                    }
                    xx += 1;
                }
            }
            let thr = &t.groups[g];
            let x0 = xx;
            if per {
                for &c in thr {
                    col(scr, xx, lvl(c));
                    xx += 1;
                }
            } else {
                col(scr, xx, thr.iter().map(|&c| lvl(c)).max().unwrap_or(0));
                xx += 1;
            }
            if lab {
                // the core number under its columns (every other core where they'd touch)
                let cw = xx - x0;
                let num = f!("{g}");
                let wide = f!("{}", n - 1).n;
                let show = wide < cw + cg || (g % 2 == 0 && wide < 2 * cw + cg);
                let mut lx = x0;
                if show {
                    lx = scr.text(x0, y + hb, num.s(), th.dim, bg, x0 + num.n);
                }
                while lx < xx {
                    scr.put(lx, y + hb, ' ', 0, bg);
                    lx += 1;
                }
            }
        }
        xx - x
    }

    /// How the threads' bars fit in `w` cells: (a column per thread, else per core;
    /// gap between cores; gap between CCDs).
    fn ov_bar_cols(&self, w: usize) -> (bool, usize, usize) {
        let t = &self.s.t;
        let n = t.groups.len();
        let nth: usize = t.groups.iter().map(|g| g.len()).sum();
        let nd = t.ndom.max(1);
        let fits = |per: bool, cg: usize, dg: usize| (if per { nth } else { n }) + n.saturating_sub(nd) * cg + (nd - 1) * dg <= w;
        [(true, 1, 2), (true, 0, 2), (true, 0, 1), (false, 1, 2), (false, 0, 1), (false, 0, 0)]
            .into_iter()
            .find(|&(p, c, d)| fits(p, c, d))
            .unwrap_or((false, 0, 0))
    }

    /// The mini bars' fill in eighths of a cell (`hb` rows) from the threads' moving
    /// bars; in smooth mode a level changes only once its bar is a whole eighth away,
    /// and doesn't turn back within 20 frames (a bar's edge never blinks). Returns
    /// their memo key.
    fn ov_levels(&mut self, hb: usize) -> u64 {
        let e = (8 * hb) as f32;
        let (sm, fr) = (self.smooth(), self.frame);
        let lv = &mut self.ov.lv;
        lv.resize(self.bu.len(), (0, 0, 0));
        let mut k = hb as u64;
        for (c, l) in lv.iter_mut().enumerate() {
            let x = self.bu[c].clamp(0.0, 1.0) * e;
            let t = x.round() as u16;
            // (moved a whole eighth, or at rest: exact)
            if !sm || (x - l.0 as f32).abs() >= 1.0 || self.vu[c] == 0.0 {
                let d = (t > l.0) as i8 - (t < l.0) as i8;
                if (!sm || d == l.2 || fr.wrapping_sub(l.1) >= 20 || l.2 == 0) && d != 0 {
                    *l = (t, fr, d);
                }
            }
            k = mixk(&[k, l.0 as u64]);
        }
        k
    }

    /// CPU panel rows: (statistics shown, bar rows, core numbers, chart rows).
    fn ov_cpu_geo(&self, ih: usize) -> ([bool; 4], usize, bool, usize, bool) {
        let can = self.ov_cpu_stats();
        let mut left = ih.saturating_sub(1);
        let lab = left >= 2;
        left -= lab as usize;
        let mut st = [false; 4];
        for k in 0..4 {
            if can[k] && left > 0 && (left >= 6 || k < 2) {
                st[k] = true;
                left -= 1;
            }
        }
        let mut hb = 1;
        let mut ch = 0;
        if left >= 4 {
            // bars grow with the room (to 4 rows), the chart takes the rest
            let g = ((left - 4) / 3).min(3);
            hb += g;
            ch = left - g;
        } else {
            hb += left.min(2);
        }
        // a caption over the bars when the chart can spare a row
        let cap = ch >= 9;
        (st, hb, lab, ch - cap as usize, cap)
    }

    fn ov_cpu(&mut self, scr: &mut Screen, r: Rect, full: bool, graphs: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let (st, hb, lab, ch, cap) = self.ov_cpu_geo(ih);
        let (u, avg, top) = self.totals();
        let maxf = self.s.t.maxf.max(1.0);
        let hot = (0..self.s.temps.len()).map(|k| self.temp(k).1).fold(0.0, f32::max);
        let z = &self.z;
        let pkg = z.role(R_PKG).filter(|&i| z.cpu_power == P_OK && z.rd[i].ok).map(|i| self.zv(i).n);
        let tk = mixk(&[(u * 100.0).round() as u64, (avg / 10.0) as u64, (top / 10.0) as u64, hot.round() as u64, pkg.map_or(0, |w| w.round() as u64 + 1), r.w as u64]);
        if self.omemo(O_CPU_T, tk) || full {
            let th = &self.t;
            let pu = num(" ", u * 100.0, 0, "% busy");
            let pc = num(" ", u * 100.0, 0, "%");
            let (ga, gt) = (ghz(avg), ghz(top));
            let a1 = fc(&[" · avg ", ga.s(), " GHz"]);
            let t1 = fc(&[" · top ", gt.s(), " GHz"]);
            let at = fc(&[" · ", ga.s(), "/", gt.s(), " GHz"]);
            let w = pkg.map_or_else(F::new, |w| fc(&[" · ", crate::ui::watts(w).s()]));
            let tc = degc(hot);
            let tt = if hot > 0.0 { fc(&[" · ", tc.s()]) } else { F::new() };
            let (cu, cf, ct) = (th.load.at(u) | BOLD, th.freq.at(avg / maxf) | BOLD, self.hcol(level(hot, 85.0, 95.0)) | BOLD);
            let lab = ("CPU", th.title | BOLD);
            let vs: [&[(&str, u32)]; 5] = [
                &[lab, (pu.s(), cu), (a1.s(), cf), (t1.s(), th.freq.at(top / maxf)), (w.s(), th.accent), (tt.s(), ct)],
                &[lab, (pu.s(), cu), (at.s(), cf), (w.s(), th.accent), (tt.s(), ct)],
                &[lab, (pu.s(), cu), (at.s(), cf), (tt.s(), ct)],
                &[lab, (pu.s(), cu), (tt.s(), ct)],
                &[lab, (pc.s(), cu)],
            ];
            self.ov_title(scr, r, full, &vs, PG_CPU);
            if full && ch >= 3 {
                self.time_axis(scr, r, ir.x + 5, iw.saturating_sub(5), 2.0, "");
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let bg = surf(S_PANEL);
        let maxx = ir.x + iw;
        // the threads' bars
        let bk = mixk(&[self.ov_levels(hb), iw as u64]);
        let by = ir.y + cap as usize;
        if self.omemo(O_CPU_B, bk) || full {
            let per = self.bu.len() > self.ngroups() && self.ov_bar_cols(iw).0;
            if cap {
                let c = if per { "each bar is a thread, two per core · cores numbered below" } else { "each bar is a core (its busiest thread)" };
                Self::line(scr, ir.x, ir.y, maxx, bg, &[(c, self.t.dim)]);
            }
            let bw = self.ov_bars(scr, ir.x, by, iw, hb, lab);
            for y in by..by + hb + lab as usize {
                Self::line(scr, ir.x + bw, y, maxx, bg, &[]);
            }
        }
        // statistics
        let y0 = by + hb + lab as usize;
        let rk = mixk(&[tk, fill16(u, 40) as u64, fill16(avg / maxf, 40) as u64, iw as u64]);
        if self.omemo(O_CPU_R, rk) || full {
            let th = &self.t;
            let mut y = y0;
            let note1 = f!("of all {} threads", self.s.f.len());
            let bw = Self::ov_bw(iw, 24);
            if st[0] {
                let p = num("", u * 100.0, 0, "%");
                self.orow(scr, ir.x, y, maxx, "busy", (p.s(), th.load.at(u) | BOLD), Some((u, &th.load, bw)), (note1.s(), th.dim));
                y += 1;
            }
            if st[1] {
                let lv = level(hot, 85.0, 95.0);
                let v = degc(hot);
                // (a number alone says nothing: normal or not, and against what)
                let note = ["normal · limit 95 °C", "warm · limit 95 °C", "limited by temperature"][lv as usize];
                let c = if lv > 0 { self.hcol(lv) } else { th.dim };
                self.orow(scr, ir.x, y, maxx, "temp", (v.s(), self.hcol(lv) | BOLD), Some(((hot - 30.0) / 65.0, &th.temp, bw)), (note, c));
                y += 1;
            }
            if st[2] {
                let v = fc(&[ghz(avg).s(), " GHz"]);
                let note = fc(&["avg · fastest core ", ghz(top).s()]);
                self.orow(scr, ir.x, y, maxx, "clock", (v.s(), th.freq.at(avg / maxf) | BOLD), Some((avg / maxf, &th.freq, bw)), (note.s(), th.dim));
                y += 1;
            }
            if st[3] {
                let v = pkg.map_or(F::new(), crate::ui::watts);
                self.orow(scr, ir.x, y, maxx, "power", (v.s(), th.accent | BOLD), None, ("CPU package", th.dim));
            }
        }
        if ch < 3 || !(graphs || full) {
            return;
        }
        let (gy, ch) = self.ov_caption(scr, ir, ch, full, &[("CPU load over time", self.t.mid), (" · all threads", self.t.dim)]);
        let k = mixk(&[self.body_key(0), ch as u64, iw as u64]);
        if self.omemo(O_CPU_G, k) || full {
            let mut vals = std::mem::take(&mut self.vals);
            let gw = iw.saturating_sub(5);
            self.hist_vals(&mut vals, 2 * gw, 0.0, 1.0, Hist::Tot);
            self.axis_graph(scr, ir.x, gy, iw, ch, &vals, 1.0, &self.t.load, &[(1.0, "100%"), (0.5, "50%"), (0.0, "0%")]);
            self.vals = vals;
        }
    }

    /// The RAM headline and its colour level: (in use %, verdict, level of in use,
    /// level overall).
    fn ov_ram(&self) -> (f32, &'static str, u8, u8) {
        let total = (self.m.mi.total as f32).max(1.0);
        let up = self.ma.used.n / total;
        let lv = self.ram_level();
        let ul = level(up, RAM_WARN, RAM_CRIT);
        let verdict = match (lv, ul) {
            (0, _) => " · plenty free",
            (1, 1) => " · getting full",
            (1, _) => " · RAM is tight",
            (_, 2) => " · nearly full",
            _ => " · short of RAM",
        };
        (up, verdict, ul, lv)
    }

    /// Swap in a few words: ("8.59 GB", "no activity") etc.; None without swap.
    fn ov_swap(&self) -> Option<(F, &'static str, u8)> {
        let mi = &self.m.mi;
        if mi.swap_total == 0 {
            return None;
        }
        let a = &self.ma;
        let v = fc(&[of_size(a.swap.n, mi.swap_total as f32).s()]);
        let io = a.swio.n;
        let (w, lv) = if io >= SWAP_WARN {
            (" · active", level(io, SWAP_WARN, SWAP_CRIT))
        } else if io > 0.0 {
            (" · little activity", 0)
        } else {
            (" · no activity (fine)", 0)
        };
        Some((v, w, lv))
    }

    fn ov_mem(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let total = (self.m.mi.total as f32).max(1.0);
        let (up, verdict, ul, lv) = self.ov_ram();
        let a = &self.ma;
        // (keys at the resolution shown)
        let q = |v: f32| (v / bstep(v)).round() as u64;
        let tk = mixk(&[q(a.used.n), lv as u64, r.w as u64]);
        let rows = if !(5..12).contains(&ih) { ih.min(3) } else { 2 };
        let ch = ih.saturating_sub(rows);
        let chart = ch >= 3 && iw >= 30;
        if self.omemo(O_MEM_T, tk) || full {
            let th = &self.t;
            let p = num(" ", up * 100.0, 0, "% in use");
            let ps = num(" ", up * 100.0, 0, "%");
            let ut = fc(&[" · ", of_size(self.ma.used.n, total).s()]);
            let (c, vc) = (self.hcol(ul) | BOLD, self.hcol(lv));
            let lab = ("RAM", th.title | BOLD);
            let vs: [&[(&str, u32)]; 4] = [&[lab, (p.s(), c), (ut.s(), th.text), (verdict, vc)], &[lab, (p.s(), c), (ut.s(), th.text)], &[lab, (p.s(), c)], &[lab, (ps.s(), c)]];
            self.ov_title(scr, r, full, &vs, PG_MEM);
            if full && chart {
                self.time_axis(scr, r, ir.x + 5, iw - 5, 2.0, "");
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let a = &self.ma;
        let k = mixk(&[tk, fill16(a.used.x / total, 40) as u64, q(a.swap.n), (a.swio.n > 0.0) as u64 + (a.swio.n >= SWAP_WARN) as u64, q(a.avail.n), iw as u64, ih as u64]);
        let maxx = ir.x + iw;
        if self.omemo(O_MEM_R, k) || full {
            let th = &self.t;
            let a = &self.ma;
            let fu = a.used.x / total;
            let uv = bsize(a.used.n, false);
            let pp = num("", up * 100.0, 0, "%");
            let of = fc(&["  of ", bsize(total, false).s()]);
            let wide = iw >= 56;
            let bw = Self::sbar_w(iw, if wide { of.n } else { 0 }).max(4);
            let mut y = ir.y;
            self.srow(scr, ir.x, y, maxx, ("in use", th.text), (uv.s(), th.fg | BOLD), (fu, bw, &th.load), (pp.s(), self.hcol(ul) | BOLD), &[(if wide { of.s() } else { "" }, th.dim)]);
            y += 1;
            let none = (0.0, 0, &th.load);
            if rows >= 2 {
                match self.ov_swap() {
                    Some((v, w, sl)) => {
                        let z = self.m.zram;
                        // (the words shortened to fit: "no activity (fine)" → "no activity")
                        let w = if LW + v.n + w.len() <= iw { w } else { w.trim_end_matches(" (fine)") };
                        let w = if LW + v.n + w.len() <= iw { w } else { "" };
                        self.srow(scr, ir.x, y, maxx, (if z { "swap zram" } else { "swap" }, th.text), (v.s(), th.text), none, ("", 0), &[(w, if sl > 0 { self.hcol(sl) } else { th.dim })]);
                    }
                    None => self.srow(scr, ir.x, y, maxx, ("swap", th.text), ("none", th.dim), none, ("", 0), &[(" no swap set up", th.dim)]),
                }
                y += 1;
            }
            if rows >= 3 {
                let v = bsize(a.avail.n, false);
                self.srow(scr, ir.x, y, maxx, ("available", th.text), (v.s(), th.fg), none, ("", 0), &[(" free for programs now", th.dim)]);
            }
        }
        if !chart {
            return;
        }
        let (then, w) = self.ram_trend(60);
        let cap = fc(&[" · ", w, " ", num("", then * 100.0, 0, "%").s(), " → ", num("", up * 100.0, 0, "%").s(), " in the last minute"]);
        let (gy, ch) = self.ov_caption(scr, ir, ch, full, &[("RAM in use over time", self.t.mid), (cap.s(), self.t.dim)]);
        let k = mixk(&[self.body_key(2), ch as u64, iw as u64]);
        if self.omemo(O_MEM_G, k) || full {
            let gw = iw - 5;
            let mut vals = std::mem::take(&mut self.vals);
            let dot = 1.0 / (4 * ch) as f32;
            let now = self.ma.used.n / total;
            if (now - self.ma.head).abs() > 0.6 * dot || !self.smooth() {
                self.ma.head = now;
            }
            self.series(&mut vals, 2 * gw, &self.m.h_used, self.ma.head);
            self.axis_graph(scr, ir.x, gy, iw, ch, &vals, 1.0, &self.t.load, &[(1.0, "100%"), (0.5, "50%"), (0.0, "0%")]);
            self.vals = vals;
        }
    }

    /// Memo key of a column chart `iw` wide: the bucket its columns are at (they
    /// step once a bucket), see `ov_cols`.
    fn ov_cols_key(&self, iw: usize, pts: u64) -> u64 {
        let b = OV_SECS;
        mixk(&[pts / b as u64, iw as u64, self.smooth() as u64 * self.body_key(2)])
    }

    /// A chart's caption line over its `ch` bottom rows of `ir` (when there are 5 or
    /// more); returns the chart's first row and height.
    fn ov_caption(&self, scr: &mut Screen, ir: Rect, ch: usize, full: bool, sp: &[(&str, u32)]) -> (usize, usize) {
        let gy = ir.y + ir.h - ch;
        if ch < 5 {
            return (gy, ch);
        }
        if full || !self.anim {
            Self::line(scr, ir.x, gy, ir.x + ir.w, surf(S_PANEL), sp);
        }
        (gy + 1, ch - 1)
    }

    /// The fullest filesystem: index.
    fn ov_fullest(&self) -> Option<usize> {
        (0..self.m.fs.len()).max_by_key(|&i| (self.m.fs[i].pct(), self.m.fs[i].mnt == "/"))
    }

    /// Stacked rate columns (lower part `a`, upper `b`, newest at the right) over the
    /// panel's bottom rows `y..y + ch` with a y-axis of round rates: column `c` (0 =
    /// now) from `col`. Returns (x of the first column, columns, seconds a column).
    #[allow(clippy::too_many_arguments)]
    fn ov_cols(&self, scr: &mut Screen, ir: Rect, y: usize, ch: usize, col: &dyn Fn(usize, usize) -> Option<(f32, f32)>, cols: (u32, u32), full: bool, net: bool, pts: u64) -> (usize, usize, usize) {
        let hold = &self.ov.head[net as usize];
        let gut = 10;
        let gx = ir.x + gut;
        let gw = ir.w.saturating_sub(gut).clamp(1, 256);
        let b = OV_SECS;
        let mut v = [(0.0f32, 0.0f32); 256];
        let mut have = 0;
        let mut top = 0.0f32;
        for (c, slot) in v.iter_mut().enumerate().take(gw) {
            match col(c, b) {
                Some(x) => {
                    *slot = x;
                    have = c + 1;
                    if c > 0 {
                        top = top.max(x.0 + x.1);
                    }
                }
                None => break,
            }
        }
        // (the network's from 100 kB/s: a quiet link stays a low line, as it reads)
        let scale = |t: f32| if net { net_scale(t.max(1e5)) } else { nice_rate(t) };
        let sc = scale(top.max(v[0].0 + v[0].1));
        // (smooth: the scale changes when the columns step anyway, a new bucket, or
        // when it must grow and hasn't changed for a second: never twice in a row)
        let st = &self.ov.scale[net as usize];
        let (so, sf, sb) = st.get();
        let bk = pts / b as u64;
        let age = self.frame.wrapping_sub(sf);
        let sc = if !self.smooth() || (sc != so && bk != sb) || (sc > so && age >= 60) {
            if sc != so || bk != sb {
                st.set((sc, if sc != so { self.frame } else { sf }, bk));
            }
            sc
        } else {
            so
        };
        let th = &self.t;
        let bg = surf(S_PANEL);
        let (lt, lh) = if net { (dtick(sc), dtick(sc / 2.0)) } else { (rate_label(sc), rate_label(sc / 2.0)) };
        let half = if ch >= 5 { ch / 2 } else { usize::MAX };
        for r in 0..ch {
            let lab = if r == 0 { lt.s() } else if r == half { lh.s() } else if r == ch - 1 { "0" } else { "" };
            let l = rpad(&f!("{}", lab), gut - 1);
            scr.text(ir.x, y + r, l.s(), th.dim, bg, gx);
            scr.put(gx - 1, y + r, if lab.is_empty() { '│' } else { '┤' }, th.border, bg);
        }
        let e8 = (ch * 8) as f32;
        let floor = if net { 1e3 } else { IDLE_BPS };
        let mut h8 = [(0u16, 0u16, false); 256];
        for (c, x) in v.iter().enumerate().take(gw) {
            let tot = x.0 + x.1;
            let t8 = if c < have && tot >= floor { ((tot / sc).min(1.0) * e8).round().max(1.0) as usize } else { 0 };
            let a8 = if tot > 0.0 { (t8 as f32 * x.0 / tot).round() as usize } else { 0 };
            h8[c] = (t8 as u16, a8 as u16, c < have);
        }
        // (smooth: the newest column, which follows the moving rates, doesn't turn
        // back within a third of a second: no edge flickers)
        let (ht, ha, hf, hd) = hold.get();
        let d = (h8[0].0 > ht) as i8 - (h8[0].0 < ht) as i8;
        if !self.smooth() || d == 0 || d == hd || self.frame.wrapping_sub(hf) >= 20 {
            if d != 0 || !self.smooth() {
                hold.set((h8[0].0, h8[0].1, self.frame, d));
            }
        } else {
            (h8[0].0, h8[0].1) = (ht, ha);
        }
        self.col_chart(scr, gx, y, ch, &h8[..gw], cols, half, full);
        (gx, gw, b)
    }

    fn ov_disk(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let nd = self.m.disks.len();
        let (mut rd, mut wr, mut busy) = (0.0f32, 0.0f32, 0.0f32);
        for v in &self.ma.disk {
            rd += v[D_RD].n;
            wr += v[D_WR].n;
            busy = busy.max(v[2].n);
        }
        let idle = rd < IDLE_BPS && wr < IDLE_BPS;
        let fi = self.ov_fullest();
        let rows = (nd > 0) as usize + fi.is_some() as usize;
        let rows = rows.min(ih);
        let ch = ih.saturating_sub(rows);
        let chart = nd > 0 && ch >= 3 && iw >= 30;
        let q = |v: f32| (v / 1e5).round() as u64;
        let tk = mixk(&[q(rd), q(wr), (busy * 100.0).round() as u64, r.w as u64, chart as u64]);
        let (rc, wc) = self.rw_cols();
        if self.omemo(O_DISK_T, tk) || full {
            let th = &self.t;
            let lab = (if nd > 1 { "disks" } else { "disk" }, th.title | BOLD);
            if nd == 0 {
                // (no disks here, a container: the filesystems only)
                self.ov_title(scr, r, full, &[&[("storage", th.title | BOLD)]], PG_MEM);
            } else if idle {
                self.ov_title(scr, r, full, &[&[lab, (" idle", th.good), (" · nothing being read or written", th.dim)], &[lab, (" idle", th.good)]], PG_MEM);
            } else {
                let (r1, w1) = (fc(&[" reading ", rate(rd, false).s()]), fc(&[" · writing ", rate(wr, false).s()]));
                let bz = num(" · ", busy * 100.0, 0, "% busy");
                let bc = th.load.at(busy);
                let w2 = fc(&[" writing ", rate(wr, false).s()]);
                let one = if rd >= wr { (r1.s(), rc | BOLD) } else { (w2.s(), wc | BOLD) };
                self.ov_title(scr, r, full, &[&[lab, (r1.s(), rc | BOLD), (w1.s(), wc | BOLD), (bz.s(), bc)], &[lab, (r1.s(), rc | BOLD), (w1.s(), wc | BOLD)], &[lab, one], &[lab]], PG_MEM);
            }
            if full && chart {
                let gw = iw.saturating_sub(10).clamp(1, 256);
                let b = OV_SECS;
                self.time_axis(scr, r, ir.x + 10, gw, b as f32, "");
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let maxx = ir.x + iw;
        let fk = fi.map_or(0, |i| mixk(&[self.ma.fs.get(i).map_or(0, |v| v.n.to_bits() as u64), self.m.fs[i].total, i as u64]));
        let k = mixk(&[tk, fk, iw as u64, ih as u64, fill16(busy, 40) as u64]);
        if self.omemo(O_DISK_R, k) || full {
            let th = &self.t;
            let mut y = ir.y;
            let bw = Self::ov_bw(iw, 13);
            if nd > 0 && y < ir.y + rows {
                let p = num("", busy * 100.0, 0, "%");
                self.orow(scr, ir.x, y, maxx, "busy", (p.s(), th.load.at(busy) | BOLD), Some((busy, &th.load, bw)), ("of the time", th.dim));
                y += 1;
            }
            if let Some(i) = fi.filter(|_| y < ir.y + rows) {
                let f = &self.m.fs[i];
                let used = self.ma.fs.get(i).map_or(f.used as f32, |v| v.n);
                let frac = used / (used + f.avail as f32).max(1.0);
                let pct = f.pct();
                let lv = level(pct as f32, FS_WARN, FS_CRIT);
                let p = f!("{pct}% full");
                // (the mount point as the label when it fits, else after the free space)
                // (the root filesystem in words, as the memory page names it)
                let root = f.mnt == "/";
                let short = root || f.mnt.chars().count() < LW;
                let free = fc(&[bsize(f.avail as f32, false).s(), " free"]);
                let note = if short { free } else { fc(&[free.s(), " · ", &f.mnt]) };
                let lab = if root { "system /" } else if short { &f.mnt } else { "fullest" };
                self.orow(scr, ir.x, y, maxx, lab, (p.s(), self.hcol(lv) | BOLD), Some((frac, &th.load, bw)), (note.s(), th.text));
            }
        }
        if !chart {
            return;
        }
        let (gy, ch) = self.ov_caption(scr, ir, ch, full, &[("■ ", rc), ("reading  ", self.t.text), ("■ ", wc), ("writing", self.t.text), (" (stacked)", self.t.dim)]);
        let k = mixk(&[self.ov_cols_key(iw, self.m.hist_n), (rd + wr).to_bits() as u64, ch as u64]);
        if self.omemo(O_DISK_G, k) || full {
            self.ov_cols(scr, ir, gy, ch, &|c, b| self.disk_col(c, b).map(|x| (x.rd, x.wr)), (rc, wc), full, false, self.m.hist_n);
        }
    }

    fn ov_net(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let now = crate::term::mono();
        let n = &self.n;
        let main = self.main_link();
        let (mut dn, mut up, mut bad, mut nup) = (0.0f32, 0.0f32, false, 0);
        for i in n.ifs.iter().filter(|i| i.prim && i.up) {
            let v = self.link_vals(&i.name);
            dn += v[0].n;
            up += v[1].n;
            bad |= i.bad(now);
            nup += 1;
        }
        let ctr = !n.ctrs.is_empty();
        let rows = (1 + ctr as usize).min(ih);
        let ch = ih.saturating_sub(rows);
        let chart = main.is_some() && ch >= 3 && iw >= 30;
        let q = |v: f32| (v / bstep(v)).round() as u64 ^ (bstep(v) as u64) << 20;
        let tk = mixk(&[q(dn), q(up), bad as u64, nup, r.w as u64, chart as u64]);
        let (cd, cu) = (self.t.accent, self.t.accent2);
        if self.omemo(O_NET_T, tk) || full {
            let th = &self.t;
            let (d, u) = (nrate(dn), nrate(up));
            let lab = ("network", th.title | BOLD);
            let vs: [&[(&str, u32)]; 2] = [
                &[lab, (" ↓ ", cd), (d.s(), cd | BOLD), (" download", th.dim), (" · ↑ ", cu), (u.s(), cu | BOLD), (" upload", th.dim), (if bad { " · packets lost" } else { "" }, th.warn)],
                &[lab, (" ↓ ", cd), (d.s(), cd | BOLD), (" ↑ ", cu), (u.s(), cu | BOLD)],
            ];
            if nup == 0 {
                self.ov_title(scr, r, full, &[&[lab, (" not connected", th.crit | BOLD)]], PG_NET);
            } else if dn + up < QUIET_BPS && !bad {
                self.ov_title(scr, r, full, &[&[lab, (" quiet", th.good), (" · ↓ ", cd), (d.s(), cd), (" ↑ ", cu), (u.s(), cu)], &[lab, (" quiet", th.good)]], PG_NET);
            } else {
                self.ov_title(scr, r, full, &vs, PG_NET);
            }
            if full && chart {
                let gw = iw.saturating_sub(10).clamp(1, 256);
                let b = OV_SECS;
                self.time_axis(scr, r, ir.x + 10, gw, b as f32, "");
            }
        }
        if ih == 0 || iw < 12 {
            return;
        }
        let maxx = ir.x + iw;
        let n = &self.n;
        let (mut bad_c, mut rest, mut checked) = (0u32, 0u32, 0u32);
        for c in &n.ctrs {
            bad_c += (c.health == 3) as u32;
            checked += (c.health > 0) as u32;
            rest += (c.state == 1) as u32;
        }
        let k = mixk(&[tk, main.map_or(0, |m| name_key(&n.ifs[m].addr) ^ n.ifs[m].speed as u64), n.ctrs.len() as u64, n.stopped as u64, (bad_c * 1000 + rest * 31 + checked) as u64, iw as u64, ih as u64]);
        if self.omemo(O_NET_R, k) || full {
            let (th, n) = (&self.t, &self.n);
            let mut y = ir.y;
            if let Some(m) = main.filter(|_| rows > 0) {
                let f = &n.ifs[m];
                let sp = if f.speed > 0 { fc(&[" · ", link_speed(f.speed).s()]) } else { F::new() };
                let ad = if f.addr.is_empty() || iw < 44 { F::new() } else { f!(" · {}", f.addr) };
                let kind = crate::net::KIND.get(f.kind as usize).copied().unwrap_or("");
                Self::row(scr, ir.x, y, maxx, &[(&f.name, th.fg | BOLD, 0), (" ", 0, 0), (kind, th.mid, 0), (sp.s(), th.text, 0), (ad.s(), th.dim, 0)]);
                y += 1;
            } else if rows > 0 {
                Self::row(scr, ir.x, y, maxx, &[("no network link is connected", th.crit, 0)]);
                y += 1;
            }
            if ctr && y < ir.y + rows {
                let c = num(" ", n.ctrs.len() as f32, 0, " running");
                let s = num(" · ", n.stopped as f32, 0, " stopped");
                let hb = num(" · ", bad_c as f32, 0, " unhealthy");
                let hr = num(" · ", rest as f32, 0, " restarting");
                let state = if bad_c > 0 {
                    (hb.s(), th.crit | BOLD)
                } else if rest > 0 {
                    (hr.s(), th.warn | BOLD)
                } else if checked > 0 {
                    (" · all healthy", th.good)
                } else {
                    ("", 0)
                };
                Self::row(scr, ir.x, y, maxx, &[("containers", th.text, 0), (c.s(), th.fg | BOLD, 0), (state.0, state.1, 0), (if n.stopped > 0 { s.s() } else { "" }, th.dim, 0)]);
            }
        }
        if !chart {
            return;
        }
        let Some(m) = main else { return };
        let nm = f!("{} traffic  ", self.n.ifs[m].name);
        let (gy, ch) = self.ov_caption(scr, ir, ch, full, &[(nm.s(), self.t.mid), ("■ ", cd), ("download  ", self.t.text), ("■ ", cu), ("upload", self.t.text)]);
        let k = mixk(&[self.ov_cols_key(iw, self.n.pts), (dn + up).to_bits() as u64, ch as u64]);
        if self.omemo(O_NET_G, k) || full {
            self.ov_cols(scr, ir, gy, ch, &|c, b| self.link_col(m, c, b), (cd, cu), full, true, self.n.pts);
        }
    }

    /// A program row's name: "postgres ×27".
    fn ov_pname(&self, key: u64, n: f32) -> F {
        match self.ov.names.iter().find(|x| x.0 == key) {
            Some((_, s)) if n >= 1.5 => fc(&[s, " ×", num("", n, 0, "").s()]),
            Some((_, s)) => fc(&[s]),
            None => F::new(),
        }
    }

    fn ov_procs(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let rows = ih.saturating_sub(1);
        let two = iw >= 56;
        let mut k = mixk(&[iw as u64, ih as u64, self.ov.names.len() as u64, self.ov.d.is_some() as u64]);
        for l in [&self.ov.busy, &self.ov.big] {
            k = mixk(&[k, l.key(8, 100.0)]);
        }
        if !(self.omemo(O_PROC, k) || full) {
            return;
        }
        let ord = self.ov.busy.order();
        let first = ord.first().map(|&i| &self.ov.busy.it[i]);
        let th = &self.t;
        let lab = ("programs", th.title | BOLD);
        let (bn, bp) = match first {
            Some(it) => (self.ov_pname(it.key, it.v[2].to), fc(&[" ", cpct(it.v[0].n).s(), " of the CPU"])),
            None => (F::new(), F::new()),
        };
        let bs = fc(&[" · busiest: ", bn.s()]);
        let b1 = f!(" · busiest: {}", bn.s().split(" ×").next().unwrap_or(""));
        let cb = th.load.at((first.map_or(0.0, |i| i.v[0].n) / 100.0).min(1.0)) | BOLD;
        let vs: [&[(&str, u32)]; 4] = [&[lab, (bs.s(), th.text), (bp.s(), cb)], &[lab, (bs.s(), th.text)], &[lab, (b1.s(), th.text)], &[lab]];
        let title: &[&[(&str, u32)]] = if bn.n == 0 { &vs[3..] } else { &vs };
        self.ov_title(scr, r, full, title, PG_PROC);
        if ih == 0 || iw < 12 {
            return;
        }
        let th = &self.t;
        let maxx = ir.x + iw;
        let (w1, x2) = if two { (iw / 2 - 1, ir.x + iw / 2 + 1) } else { (iw, maxx) };
        let vw = 9;
        if two {
            Self::row(scr, ir.x, ir.y, ir.x + w1, &[("busiest", th.mid, (w1 - vw) as i32), ("CPU", th.dim, -(vw as i32))]);
            Self::row(scr, ir.x + w1, ir.y, x2, &[]);
            Self::row(scr, x2, ir.y, maxx, &[("biggest", th.mid, (maxx - x2 - vw) as i32), ("memory", th.dim, -(vw as i32))]);
        } else {
            let nw = iw.saturating_sub(2 * vw + 1) as i32;
            Self::row(scr, ir.x, ir.y, maxx, &[("busiest", th.mid, nw), ("CPU", th.dim, -(vw as i32)), ("memory", th.dim, -(vw as i32 + 1))]);
        }
        let list = |l: &List, x: usize, w: usize, mem: bool, scr: &mut Screen| {
            let ord = l.order();
            for j in 0..rows {
                let y = ir.y + 1 + j;
                let Some(it) = ord.get(j).map(|&i| &l.it[i]) else {
                    Self::row(scr, x, y, x + w, &[]);
                    continue;
                };
                let mut name = self.ov_pname(it.key, it.v[2].to);
                let nw0 = w.saturating_sub(if two { 9 } else { 19 });
                if name.s().chars().count() > nw0 && nw0 > 2 {
                    // (cut with an ellipsis)
                    let cut: String = name.s().chars().take(nw0 - 1).collect();
                    name = fc(&[&cut, "…"]);
                }
                let c = it.v[0].n;
                let cpu = cpct(c);
                let ms = bsize(it.v[1].n, false);
                let cc = th.load.at((c / 100.0).min(1.0));
                let nw = w.saturating_sub(if two { vw } else { 2 * vw + 1 }) as i32;
                if two {
                    let (v, vc) = if mem { (ms.s(), th.fg) } else { (cpu.s(), cc | BOLD) };
                    Self::row(scr, x, y, x + w, &[(name.s(), th.text, nw), (v, vc, -(vw as i32))]);
                } else {
                    Self::row(scr, x, y, x + w, &[(name.s(), th.text, nw), (cpu.s(), cc | BOLD, -(vw as i32)), (ms.s(), th.mid, -(vw as i32 + 1))]);
                }
            }
        };
        if self.ov.d.is_none() || self.ov.names.is_empty() {
            Self::row(scr, ir.x, ir.y + 1, maxx, &[("reading processes…", th.dim, 0)]);
            return;
        }
        list(&self.ov.busy, ir.x, w1, false, scr);
        if two {
            for j in 0..rows {
                Self::row(scr, ir.x + w1, ir.y + 1 + j, x2, &[]);
            }
            list(&self.ov.big, x2, maxx - x2, true, scr);
        }
    }

    /// Power in a few words: "CPU 87 W · GPU 12 W · battery 8.2 W 54%".
    fn ov_power(&self) -> (F, u32) {
        let z = &self.z;
        let th = &self.t;
        let mut f = F::new();
        let mut sep = "";
        for (ro, lab) in [(R_PKG, "CPU "), (R_PSYS, "computer "), (R_GPU, "GPU ")] {
            // (an idle built-in GPU's 0.02 W says nothing)
            if let Some(i) = z.role(ro).filter(|&i| z.rd[i].ok && (ro != R_GPU || z.rd[i].val >= 0.5)) {
                f.str(sep);
                f.str(lab);
                f.str(crate::ui::watts(self.zv(i).n).s());
                sep = " · ";
            }
        }
        for b in &z.bats {
            f.str(sep);
            f.str("battery ");
            f.str(num("", b.pct.max(0.0), 0, "%").s());
            match (b.st, z.role(R_BAT).filter(|&i| z.rd[i].ok)) {
                (1, _) => f.str(" charging"),
                (2, Some(i)) => {
                    f.str(" · ");
                    f.str(crate::ui::watts(self.zv(i).n).s());
                }
                _ => {}
            }
            sep = " · ";
        }
        if f.n == 0 && z.cpu_power == P_LOCKED {
            return (f!("CPU watts: run cpuview --setup-sensors"), th.dim);
        }
        (f, th.accent)
    }

    fn ov_sens(&mut self, scr: &mut Screen, r: Rect, full: bool) {
        let ir = r.inner();
        let (iw, ih) = (ir.w, ir.h);
        let hot = self.ov_hottest();
        let fan = self.ov_fan();
        let (pw, pc) = self.ov_power();
        let now = crate::term::mono();
        let z = &self.z;
        let fage = fan.map_or(0, |(i, _)| z.age(z.rd[i].src as usize, now));
        let mut k = mixk(&[iw as u64, ih as u64, name_key(pw.s()), fage as u64 / 60]);
        if let Some((n, t, _, _)) = &hot {
            k = mixk(&[k, name_key(n.s()), t.to_bits() as u64]);
        }
        if let Some((i, s)) = fan {
            k = mixk(&[k, self.zv(i).n.to_bits() as u64, s as u64]);
        }
        if !(self.omemo(O_SENS, k) || full) {
            return;
        }
        let th = &self.t;
        let lab = ("sensors", th.title | BOLD);
        let lv = hot.as_ref().map_or(0, |h| level(h.1, h.2, h.3));
        let verdict = [(" all normal", th.good), (" warm", th.warn), (" too hot", th.crit | BOLD)][lv as usize];
        let ht = hot.as_ref().map_or(F::new(), |h| fc(&[" · hottest: ", h.0.s(), " ", degc(h.1).s()]));
        let hs = hot.as_ref().map_or(F::new(), |h| fc(&[" · ", degc(h.1).s()]));
        self.ov_title(scr, r, full, &[&[lab, verdict, (ht.s(), th.text)], &[lab, verdict, (hs.s(), th.text)], &[lab, verdict]], PG_SENS);
        if ih == 0 || iw < 12 {
            return;
        }
        let th = &self.t;
        let maxx = ir.x + iw;
        let mut y = ir.y;
        let end = ir.y + ih;
        let none = (0.0, 0, &th.load);
        if let Some((n, t, w, c)) = &hot {
            let l = level(*t, *w, *c);
            let v = degc(*t);
            let note = if l == 0 { fc(&[n.s(), " · normal, limit ", degc(*c).s()]) } else { fc(&[n.s(), " · limit ", degc(*c).s()]) };
            // (the bar's width doesn't follow the name's length)
            let bw = Self::ov_bw(iw, 28).min(12);
            let note = if note.n + 27 <= iw { note } else { fc(&[n.s()]) };
            self.orow(scr, ir.x, y, maxx, "hottest", (v.s(), self.hcol(l) | BOLD), Some((*t / c.max(1.0), &Ramp([self.hcol(l); 65]), bw)), (note.s(), th.mid));
            y += 1;
        }
        if let Some((i, stopped)) = fan.filter(|_| y < end) {
            let rr = &self.z.rd[i];
            let v = num("", self.zv(i).n, 0, " RPM");
            // (eco reads a board's sensor chip every 10 minutes: say how old the number is)
            let ago = if fage < 60 {
                F::new()
            } else if iw >= 56 && SPEEDS[self.speed].1 == 0 {
                fc(&[" · read ", ago(fage as usize / 60 * 60).s(), " (eco)"])
            } else {
                fc(&[" · read ", ago(fage as usize).s()])
            };
            let lab = if rr.name.starts_with("CPU fan") { "CPU fan" } else { "fan" };
            let note = if stopped {
                f!(" stopped!")
            } else if lab == "fan" && rr.name != "fan" {
                f!(" {}{}", rr.name, ago.s())
            } else {
                ago
            };
            self.srow(scr, ir.x, y, maxx, (lab, th.text), (v.s(), if stopped { th.crit | BOLD } else { th.fg | BOLD }), none, ("", 0), &[(note.s(), if stopped { th.crit } else { th.dim })]);
            y += 1;
        }
        if pw.n > 0 && y < end {
            Self::line(scr, ir.x, y, maxx, surf(S_PANEL), &[(f!("{:<w$}", "power", w = LW).s(), th.text), (pw.s(), pc | BOLD)]);
            y += 1;
        }
        Self::blank(scr, ir.x, &mut y, end, maxx);
    }

    /// Rows of the small-screen view: (page, rect) for each line, the bars and chart
    /// in the CPU's.
    fn ov_line_rows(&self) -> Vec<(usize, Rect)> {
        let r = self.ov.lay.cpu;
        let (hb, lab, ch) = self.ov_line_geo(r.h);
        let mut v = Vec::new();
        let mut y = r.y;
        let line = |p: usize, h: usize, y: &mut usize, v: &mut Vec<(usize, Rect)>| {
            v.push((p, Rect { x: r.x, y: *y, w: r.w, h }));
            *y += h;
        };
        line(PG_CPU, 1 + hb + lab as usize + ch + (ch > 0) as usize, &mut y, &mut v);
        for p in [PG_MEM, PG_MEM, PG_NET, PG_PROC, PG_SENS] {
            line(p, 1, &mut y, &mut v);
        }
        v.retain(|x| x.1.y + x.1.h <= r.y + r.h);
        v
    }

    /// Small screens in `h` rows: (bar rows, core numbers, chart rows). Six lines
    /// and a row of bars at least; then taller bars, the core numbers, and the CPU's
    /// load over time with its time axis.
    fn ov_line_geo(&self, h: usize) -> (usize, bool, usize) {
        let mut left = h.saturating_sub(7);
        let mut hb = 1 + (left >= 1) as usize;
        left -= hb - 1;
        let lab = left >= 1;
        left -= lab as usize;
        if (1..4).contains(&left) {
            hb += 1;
            left -= 1;
        }
        (hb, lab, if left >= 4 { left - 1 } else { 0 })
    }

    /// Small screens: one line per page ("RAM 31% in use · 9.91 of 32.2 GB"), the
    /// threads' bars under the CPU's, and its load over time if there is room.
    fn ov_lines(&mut self, scr: &mut Screen, full: bool, graphs: bool) {
        let r = self.ov.lay.cpu;
        if r.w < 8 {
            return;
        }
        let (hb, lab, ch) = self.ov_line_geo(r.h);
        let rows = self.ov_line_rows();
        let (u, avg, _) = self.totals();
        let hot = (0..self.s.temps.len()).map(|k| self.temp(k).1).fold(0.0, f32::max);
        let maxx = r.x + r.w;
        let bg = surf(S_PANEL);
        // bars (every frame they move)
        let bk = mixk(&[self.ov_levels(hb), r.w as u64, r.h as u64]);
        if self.omemo(O_CPU_B, bk) || full {
            let bw = self.ov_bars(scr, r.x + 1, r.y + 1, r.w - 1, hb, lab);
            for y in r.y + 1..r.y + 1 + hb + lab as usize {
                scr.put(r.x, y, ' ', 0, bg);
                Self::line(scr, r.x + 1 + bw, y, maxx, bg, &[]);
            }
        }
        if !(graphs || full) {
            return;
        }
        let (ul, lv) = {
            let (up, _, ul, lv) = self.ov_ram();
            let _ = up;
            (ul, lv)
        };
        let total = (self.m.mi.total as f32).max(1.0);
        let up = self.ma.used.n / total;
        // (each line the longest of its forms that fits)
        let tw = r.w.saturating_sub(7);
        let fit = |a: F, b: F| if a.s().chars().count() <= tw { a } else { b };
        let pu = num("", up * 100.0, 0, "% in use");
        let ram = fit(fc(&[pu.s(), " · ", of_size(self.ma.used.n, total).s()]), pu);
        let ht = if hot > 0.0 { fc(&[" · ", degc(hot).s()]) } else { F::new() };
        let pb = num("", u * 100.0, 0, "% busy");
        let cpu = fit(fc(&[pb.s(), " · ", ghz(avg).s(), " GHz", ht.s()]), fc(&[pb.s(), ht.s()]));
        let (mut rd, mut wr) = (0.0f32, 0.0f32);
        for v in &self.ma.disk {
            rd += v[D_RD].n;
            wr += v[D_WR].n;
        }
        let fs = self.ov_fullest().map_or(F::new(), |i| f!(" · {} {}% full", self.m.fs[i].mnt, self.m.fs[i].pct()));
        let one = if rd >= wr { fc(&["reading ", rate(rd, false).s()]) } else { fc(&["writing ", rate(wr, false).s()]) };
        let disk = if self.m.disks.is_empty() {
            f!("{}", &fs.s().get(3..).unwrap_or(""))
        } else if rd < IDLE_BPS && wr < IDLE_BPS {
            fit(fc(&["idle", fs.s()]), f!("idle"))
        } else {
            fit(fc(&["reading ", rate(rd, false).s(), " · writing ", rate(wr, false).s(), fs.s()]), fit(fc(&[one.s(), fs.s()]), one))
        };
        let (mut dn, mut upr, mut nup) = (0.0f32, 0.0f32, 0);
        for i in self.n.ifs.iter().filter(|i| i.prim && i.up) {
            let v = self.link_vals(&i.name);
            dn += v[0].n;
            upr += v[1].n;
            nup += 1;
        }
        let net = if nup == 0 { f!("not connected") } else { fc(&["↓ ", nrate(dn).s(), " ↑ ", nrate(upr).s()]) };
        let ord = self.ov.busy.order();
        let mut top = F::new();
        for (j, &i) in ord.iter().take(3).enumerate() {
            let it = &self.ov.busy.it[i];
            top.str(if j > 0 { " · " } else { "" });
            top.str(self.ov.names.iter().find(|n| n.0 == it.key).map_or("", |n| n.1.as_str()));
            top.str(" ");
            top.str(cpct(it.v[0].n).s());
        }
        let hotf = self.ov_hottest().map_or(F::new(), |h| fc(&[h.0.s(), " ", degc(h.1).s()]));
        let fan = self.ov_fan().map_or(F::new(), |(i, _)| fc(&[" · fan ", num("", self.zv(i).n, 0, " RPM").s()]));
        let sens = fc(&[hotf.s(), fan.s()]);
        let k = mixk(&[name_key(cpu.s()), name_key(ram.s()), name_key(disk.s()), name_key(net.s()), name_key(top.s()), name_key(sens.s()), r.w as u64, self.body_key(0)]);
        if !(self.omemo(O_LINES, k) || full) {
            return;
        }
        let th = &self.t;
        let tl = |s: &'static str| (s, th.title | BOLD);
        let lc = self.hcol(level(hot, 85.0, 95.0));
        let texts: [[(&str, u32); 2]; 6] = [
            [tl(" CPU  "), (cpu.s(), th.load.at(u) | BOLD)],
            [tl(" RAM  "), (ram.s(), self.hcol(ul.max(lv)))],
            [tl(" disk "), (disk.s(), th.text)],
            [tl(" net  "), (net.s(), if nup == 0 { th.crit } else { th.text })],
            [tl(" top  "), (top.s(), th.text)],
            [tl(" hot  "), (sens.s(), lc)],
        ];
        for (j, (_, rr)) in rows.iter().enumerate() {
            Self::line(scr, r.x, rr.y, maxx, bg, &texts[j]);
        }
        // CPU load over time under the bars, with its time axis
        if let Some((_, cr)) = rows.first().filter(|_| ch > 0) {
            let gy = cr.y + 1 + hb + lab as usize;
            let mut vals = std::mem::take(&mut self.vals);
            let gw = r.w.saturating_sub(6);
            self.hist_vals(&mut vals, 2 * gw, 0.0, 1.0, Hist::Tot);
            self.axis_graph(scr, r.x, gy, r.w, ch, &vals, 1.0, &self.t.load, &[(1.0, "100%"), (0.0, "0%")]);
            self.vals = vals;
            let ay = gy + ch;
            let ar = Rect { x: r.x.saturating_sub(1), y: ay - 1, w: r.w + 2, h: 2 };
            for x in r.x..maxx {
                scr.put(x, ay, ' ', 0, bg);
            }
            self.time_axis(scr, ar, r.x + 5, gw, 2.0, "CPU load");
            // (time_axis draws a border line: blank it back under the labels)
            for x in r.x..maxx {
                let i = ay * scr.w + x;
                if scr.buf.get(i).is_some_and(|c| c.ch == '─' || c.ch == '╰' || c.ch == '╯') {
                    scr.put(x, ay, ' ', 0, bg);
                }
            }
        }
    }
}

/// Heights for panels stacked in `h` rows, each (min, comfortable, want, max): the
/// minimums in order (a panel that doesn't fit is left out), then comfortable
/// heights, then what they want, then the rest shared among those under their
/// maximum.
fn ostack(h: usize, ps: &[(usize, usize, usize, usize)]) -> Vec<usize> {
    let mut hs = vec![0; ps.len()];
    let mut left = h;
    for (k, p) in ps.iter().enumerate() {
        if p.0 > 0 && p.0 <= left {
            hs[k] = p.0;
            left -= p.0;
        }
    }
    for tier in [1, 2] {
        for (k, p) in ps.iter().enumerate() {
            if hs[k] > 0 {
                let g = if tier == 1 { p.1 } else { p.2 }.saturating_sub(hs[k]).min(left);
                hs[k] += g;
                left -= g;
            }
        }
    }
    while left > 0 {
        let open: Vec<usize> = (0..ps.len()).filter(|&k| hs[k] > 0 && hs[k] < ps[k].3).collect();
        if open.is_empty() {
            break;
        }
        let each = (left / open.len()).max(1);
        for k in open {
            let g = each.min(ps[k].3 - hs[k]).min(left);
            hs[k] += g;
            left -= g;
        }
    }
    hs
}

/// A program's key (its name).
fn name_key(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacking_gives_minimums_then_wants_then_the_rest() {
        assert_eq!(ostack(20, &[(3, 5, 10, 100), (3, 4, 6, 8)]), vec![12, 8]);
        assert_eq!(ostack(5, &[(3, 5, 10, 100), (3, 4, 6, 8)]), vec![5, 0]);
        assert_eq!(ostack(9, &[(3, 5, 10, 100), (3, 4, 6, 8)]), vec![5, 4]);
        assert_eq!(ostack(12, &[(3, 5, 10, 100), (3, 4, 6, 8)]), vec![8, 4]);
    }
}

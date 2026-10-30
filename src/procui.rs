//! View 4: processes. A table of every process (flat, grouped by program, or as a
//! tree), sorted with hysteresis so near-ties don't flip-flop, with a detail strip for
//! the selected one (whole command line, working directory, cgroup / container, CPU
//! and memory over the last minute), and guarded actions (signals, nice).
//!
//! Smooth mode: CPU % and memory digits morph and bars spring (`anim::Val`); rows
//! slide to a new place over ~0.3 s (each row's position is a spring towards its index,
//! and the rows on screen are laid out in order of position, so none ever overlaps or
//! leaves a gap); new processes fade in, exited ones fade out. Data: see `procs`.

use super::*;
use crate::anim::{spring, Val, W_LOAD};
use crate::color::mix;
use crate::input::Key;
use crate::procs::{self, Procs, IOK, NEW, NOIO, NORATE};

/// pages (`App::page`), numbered as their keys: 0 overview, 1 cpu ... 5 sensors
pub const PG_OVERVIEW: usize = 0;
pub const PG_CPU: usize = 1;
pub const PG_MEM: usize = 2;
pub const PG_NET: usize = 3;
pub const PG_PROC: usize = 4;
pub const PG_SENS: usize = 5;
pub const NPAGES: usize = 6;
/// (tab label, config / --page name)
pub fn page_name(p: usize) -> (&'static str, &'static str) {
    match p {
        PG_OVERVIEW => ("all", "overview"),
        PG_MEM => ("mem/disk", "memory"),
        PG_NET => ("net", "network"),
        PG_PROC => ("proc", "processes"),
        PG_SENS => ("sens", "sensors"),
        _ => ("cpu", "cpu"),
    }
}

pub fn page_index(name: &str) -> Option<usize> {
    match name {
        "overview" | "all" | "0" => Some(PG_OVERVIEW),
        "cpu" | "1" => Some(PG_CPU),
        "memory" | "mem" | "2" => Some(PG_MEM),
        "network" | "net" | "3" => Some(PG_NET),
        "processes" | "proc" | "4" => Some(PG_PROC),
        "sensors" | "sens" | "power" | "fans" | "5" => Some(PG_SENS),
        _ => None,
    }
}

/// sort columns (`o` cycles them)
pub const NSORT: usize = 5;
pub fn psort_name(s: usize) -> &'static str {
    match s {
        SO_MEM => "mem",
        SO_DISK => "disk",
        SO_PID => "pid",
        SO_NAME => "name",
        _ => "cpu",
    }
}
const SO_CPU: usize = 0;
const SO_MEM: usize = 1;
const SO_DISK: usize = 2;
const SO_PID: usize = 3;
const SO_NAME: usize = 4;

/// columns
const C_PID: usize = 0;
const C_USER: usize = 1;
const C_CPU: usize = 2;
const C_BAR: usize = 3;
const C_MEM: usize = 4;
const C_RAM: usize = 5;
const C_RD: usize = 6;
const C_WR: usize = 7;
const C_THR: usize = 8;
const C_ST: usize = 9;
const C_AGE: usize = 10;
const NC: usize = 11;
/// widths (with the space before), the bar's is set by the layout
const CW: [usize; NC] = [7, 9, 6, 0, 9, 6, 7, 7, 5, 2, 7];
/// dropped first to last as the table narrows
const DROP: [usize; 10] = [
    C_AGE, C_WR, C_RD, C_THR, C_ST, C_RAM, C_BAR, C_USER, C_PID, C_MEM,
];
/// the command column's minimum
const CMD_MIN: usize = 14;

/// row flags
const R_KIDS: u8 = 1;
const R_OPEN: u8 = 2;
const R_LAST: u8 = 4;

/// seconds a slide / fade takes
const W_SLIDE: f32 = 13.0;
const FADE_IN: f32 = 0.4;
const FADE_OUT: f32 = 0.6;
/// a changed sort value takes effect at once past its margin, else after this
const HOLD_MS: u32 = 4000;

/// A table row: a process, or a program's group of them.
#[derive(Clone, Copy, Default, Debug)]
pub struct Row {
    /// the process (`Procs::list` index); a group: its first member
    pub idx: u32,
    /// group: index into `PV::aggs` + 1 (0: a process row)
    pub g: u32,
    pub depth: u8,
    pub fl: u8,
    /// tree: bit d set = the ancestor at depth d has siblings below it
    pub guide: u16,
}

/// A program's processes (group by program).
#[derive(Clone, Default, Debug)]
pub struct Agg {
    pub key: u32,
    pub n: u32,
    pub first: u32,
    pub cpu: f32,
    pub rss: u64,
    pub rd: f32,
    pub wr: f32,
    pub thr: u32,
    pub noio: bool,
    pub iok: bool,
    pub sv: f32,
    pub svt: u32,
    /// row index now and at the previous layout
    pub rank: u32,
    pub prank: u32,
    /// members (list indices), sorted
    pub mem: Vec<u32>,
}

/// A row on screen and its animation (smooth mode).
#[derive(Clone, Copy, Debug)]
struct RA {
    key: u64,
    pid: u32,
    start: u64,
    row: Row,
    pos: f32,
    vel: f32,
    target: f32,
    cpu: Val,
    mem: Val,
    alpha: f32,
    /// still a row of the table (else: sliding out or fading out)
    live: bool,
    /// settled (no animation until new targets)
    rest: bool,
    gone: bool,
}

/// The dialog of an action.
#[derive(Clone, Debug)]
pub struct Dlg {
    /// 0 signal, 1 nice
    pub kind: u8,
    pub sig: usize,
    pub nice: i32,
    pub old_nice: i32,
    /// (pid, start) of every process it acts on
    pub who: Vec<(u32, u64)>,
    pub label: String,
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct PLay {
    pub table: Rect,
    pub detail: Rect,
    /// x offset of each column inside the table (usize::MAX: hidden), the command
    /// column's, the bar's width
    pub cx: [usize; NC],
    pub cmd_x: usize,
    pub bw: usize,
}

#[derive(Default)]
pub struct PV {
    pub d: Option<Procs>,
    pub rows: Vec<Row>,
    pub aggs: Vec<Agg>,
    pub sort: usize,
    pub rev: bool,
    pub tree: bool,
    pub group: bool,
    /// CPU % of the whole machine instead of one thread
    pub total: bool,
    pub filter: String,
    pub editing: bool,
    /// the selected row's key, its index, the first row shown
    pub sel: u64,
    pub sel_i: usize,
    /// `f`: the selection follows its process when the order changes (else it stays
    /// on its row: the table never moves by itself)
    pub pinned: bool,
    pub scroll: usize,
    /// tree: collapsed processes; grouped: expanded programs (row keys)
    folds: Vec<u64>,
    ra: Vec<RA>,
    /// row key drawn in each table slot (clicks)
    slots: Vec<u64>,
    memo: Vec<u64>,
    pub dlg: Option<Dlg>,
    msg: String,
    msg_bad: bool,
    msg_until: i64,
    pub lay: PLay,
    det: procs::Detail,
    /// processes: total, running, sleeping, disk wait, stopped, zombie, idle; threads
    cnt: [u32; 7],
    threads: u32,
    /// the visible processes (with filter), matching
    shown: u32,
    pub last_scan: i64,
    pub last_rebuild: i64,
    pub dirty: bool,
    /// bumped whenever row data changes (rows redraw only then or as they move)
    ver: u64,
    /// (prows' scratch)
    cand: Vec<u64>,
}

impl PV {
    pub fn new() -> PV {
        PV {
            // grouped by program: "postgres ×8" reads better than eight rows
            group: true,
            ..Default::default()
        }
    }
}

/// memo slots
const K_SUM: usize = 0;
const K_HEAD: usize = 1;
const K_DET: usize = 2;
const K_ROWS: usize = 4;
const K_BARS: usize = K_ROWS + 1024;

/// Unsorted list of u64 keys → ascending (heap sort: one small copy for every order).
/// `names`: order by the names of the processes the low 32 bits index instead (case
/// insensitive, then by index; `true`: reversed).
fn hsort(v: &mut [u64], names: Option<(&[procs::P], bool)>) {
    let lt = |a: u64, b: u64| -> bool {
        let Some((list, rev)) = names else {
            return a < b;
        };
        let name = |k: u64| {
            list.get((k & 0xffff_ffff) as usize)
                .map_or("", |p| p.name())
        };
        let o = name(a)
            .bytes()
            .map(|c| c.to_ascii_lowercase())
            .cmp(name(b).bytes().map(|c| c.to_ascii_lowercase()))
            .then((a & 0xffff_ffff).cmp(&(b & 0xffff_ffff)));
        (o == std::cmp::Ordering::Less) != rev
    };
    let n = v.len();
    let sift = |v: &mut [u64], mut i: usize, n: usize| loop {
        let mut m = i;
        for c in [2 * i + 1, 2 * i + 2] {
            if c < n && lt(v[m], v[c]) {
                m = c;
            }
        }
        if m == i {
            break;
        }
        v.swap(i, m);
        i = m;
    };
    for i in (0..n / 2).rev() {
        sift(v, i, n);
    }
    for end in (1..n).rev() {
        v.swap(0, end);
        sift(v, 0, end);
    }
}

/// The group a row key names (group rows' keys have the top bit set).
fn agg_of(aggs: &[Agg], key: u64) -> Option<&Agg> {
    if key >> 63 == 0 {
        return None;
    }
    aggs.iter().find(|g| g.key as u64 == key & 0xffff_ffff)
}

fn lower_has(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    n.is_empty()
        || h.windows(n.len())
            .any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// "45s", "12m", "3h05m", "4d02h".
pub fn age(s: u64) -> F {
    let mut f = F::new();
    let two = |f: &mut F, v: u64| {
        f.push(b'0' + (v / 10 % 10) as u8);
        f.push(b'0' + (v % 10) as u8);
    };
    if s < 60 {
        f.int(s as u32, 0);
        f.push(b's');
    } else if s < 3600 {
        f.int((s / 60) as u32, 0);
        f.push(b'm');
    } else if s < 86400 {
        f.int((s / 3600) as u32, 0);
        f.push(b'h');
        two(&mut f, s / 60 % 60);
        f.push(b'm');
    } else {
        f.int((s / 86400) as u32, 0);
        f.push(b'd');
        two(&mut f, s / 3600 % 24);
        f.push(b'h');
    }
    f
}

/// CPU % with one decimal below 100, whole above.
fn cpct(v: f32) -> F {
    let v = v.max(0.0);
    if v >= 99.95 {
        num("", v, 0, "")
    } else {
        let c = (v * 10.0).round() as u32;
        let mut f = F::new();
        f.int(c / 10, 0);
        f.push(b'.');
        f.push(b'0' + (c % 10) as u8);
        f
    }
}

/// Process state letter → words.
fn state_name(s: u8) -> &'static str {
    match s {
        b'R' => "running",
        b'S' => "sleeping",
        b'D' => "disk wait",
        b'Z' => "zombie",
        b'T' => "stopped",
        b't' => "traced",
        b'I' => "idle",
        b'X' => "dead",
        _ => "?",
    }
}

/// A table row written left to right: colours faded (new / exited rows), blanks up
/// to each column.
struct Pen<'a> {
    scr: &'a mut Screen,
    x: usize,
    y: usize,
    maxx: usize,
    bg: u32,
    /// the background colour faded towards, the row's opacity
    fb: u32,
    al: f32,
}

impl Pen<'_> {
    #[inline(never)]
    fn to(&mut self, x: usize) {
        while self.x < x.min(self.maxx) {
            self.scr.put(self.x, self.y, ' ', 0, self.bg);
            self.x += 1;
        }
    }
    #[inline(never)]
    fn put(&mut self, s: &str, fg: u32) {
        let fg = fade(fg, self.fb, self.al);
        self.x = self.scr.text(self.x, self.y, s, fg, self.bg, self.maxx);
    }
    /// Right-aligned, ending at `end`.
    fn right(&mut self, end: usize, s: &str, fg: u32) {
        self.to(end.saturating_sub(s.chars().count()));
        let m = self.maxx;
        self.maxx = end.min(m);
        self.put(s, fg);
        self.maxx = m;
    }
}

#[inline(never)]
fn fade(c: u32, bg: u32, a: f32) -> u32 {
    if a >= 0.999 {
        c
    } else {
        mix(bg, c & 0xff_ffff, a.max(0.0)) | (c & !0xff_ffff & !BOLD)
    }
}

/// Chart scale for a memory peak: 1, 2, 3 or 5 × 10ⁿ bytes, 1 MB at least.
fn nice_bytes(v: f32) -> f32 {
    let mut p = 1e6f32;
    loop {
        for m in [1.0f32, 2.0, 3.0, 5.0] {
            if m * p >= v || p >= 1e15 {
                return m * p;
            }
        }
        p *= 10.0;
    }
}

impl App {
    // ── data ───────────────────────────────────────────────────────────────

    fn pnow_ms() -> u32 {
        (crate::term::mono() / 1_000_000) as u32 | 1
    }

    /// Table rows visible (below the summary and header).
    fn prows_vis(&self) -> usize {
        self.pv.lay.table.inner().h.saturating_sub(2)
    }

    /// The page became visible: a fresh scan.
    pub fn procs_enter(&mut self) {
        let d = self.pv.d.get_or_insert_with(|| Procs::new("/proc"));
        d.reset();
        let now = crate::term::mono();
        d.io_all = self.pv.sort == SO_DISK;
        d.scan(now, 0);
        self.pv.last_scan = now;
        self.pv.ra.clear();
        self.prebuild(true);
        if let Some(d) = self.pv.d.as_mut() {
            d.read_tracked(now, true);
        }
    }

    /// The page was left: stop reading (and close the tracked files).
    pub fn procs_leave(&mut self) {
        if let Some(d) = self.pv.d.as_mut() {
            d.reset();
        }
        self.pv.rows = Vec::new();
        self.pv.aggs = Vec::new();
        self.pv.ra = Vec::new();
        self.pv.dlg = None;
    }

    /// One sample while the page is shown (main loop). Smooth: tracked rows every
    /// sample, a new order 2.5 times a second, a full scan about every second (more
    /// rarely if scans are slow: ≤ 0.2 % of a thread). Eco: tracked rows every 4 s, a
    /// full scan every minute or more.
    pub fn procs_sample(&mut self, now: i64, eco: bool, smooth: bool, tick: i64, push: u32) {
        const SEC: i64 = 1_000_000_000;
        let Some(d) = self.pv.d.as_mut() else {
            return;
        };
        // (Windows: no /proc/loadavg, the CPU sampler's estimate; no full process
        // lists in eco after the first, see win/procs.rs)
        #[cfg(windows)]
        {
            d.load = self.s.loadavg;
            d.set_eco(eco);
        }
        let scan_gap = if eco {
            // (eco: a minute apart or more, so scans take at most ~17 µs/s)
            (60 * SEC).max(d.cost * 60_000)
        } else {
            SEC.max(d.cost * 2500)
        };
        let first = d.scans == 1;
        let scan = now - self.pv.last_scan >= scan_gap - SEC / 5
            || (first && now - self.pv.last_scan >= SEC / 2);
        // tracked rows: smooth 2.5 times a second (their `io` with scans), eco every 4 s
        let track = if eco {
            now - d.last_track >= 4 * SEC - SEC / 5 || scan
        } else {
            tick % 4 == 0 || scan
        };
        let mut changed = false;
        if track {
            d.read_tracked(now, eco || scan);
            changed = true;
        }
        if scan {
            d.io_all = self.pv.sort == SO_DISK;
            d.scan(now, if smooth { 1500 } else { 0 });
            self.pv.last_scan = now;
        }
        if push > 0 {
            d.hist_push(push);
        }
        let resort = scan || eco || !smooth || tick % 4 == 0 || self.pv.dirty;
        if resort && changed | scan {
            self.prebuild(false);
        } else if changed {
            self.psync(false); // new targets for the numbers
        }
        // thousands of processes: the scans' and sorts' scratch memory back to the
        // system now and then (it would stay as heap)
        #[cfg(target_env = "gnu")]
        if resort && self.pv.d.as_ref().map_or(0, |d| d.list.len()) > 1500 {
            unsafe { libc::malloc_trim(0) };
        }
        // the selected process's details now and then
        let gap = if eco { 15 } else { 3 } * SEC;
        if now - self.pv.det.t >= gap {
            self.pdetail(true);
        }
    }

    /// The process `key` of a row (process rows), else None.
    fn prow_proc(&self, r: &Row) -> Option<&procs::P> {
        self.pv.d.as_ref()?.list.get(r.idx as usize)
    }

    fn row_key(&self, r: &Row) -> u64 {
        if r.g > 0 {
            (1u64 << 63) | self.pagg(r).map_or(0, |a| a.key) as u64
        } else {
            self.prow_proc(r).map_or(0, |p| p.key())
        }
    }

    /// Rebuild the rows (filter, group / tree, sort), keep the selection on its
    /// process, update the tracked set and the animation targets.
    pub fn prebuild(&mut self, snap: bool) {
        self.pv.dirty = false;
        self.pv.last_rebuild = crate::term::mono();
        let Some(mut d) = self.pv.d.take() else {
            return;
        };
        let now = Self::pnow_ms();
        let so = self.pv.sort;
        let rev = self.pv.rev;
        let filt = self.pv.filter.to_ascii_lowercase();
        let digits = filt.bytes().all(|c| c.is_ascii_digit());
        let n = d.list.len();
        // sort values with hysteresis (a change counts at once past its margin, else
        // once it has lasted HOLD_MS); counts; which processes show
        let mut vis = vec![0u8; n];
        let mut cnt = [0u32; 7];
        let mut threads = d.nk;
        for p in d.list.iter_mut() {
            let v = match so {
                SO_CPU => p.cpu,
                SO_MEM => p.rss as f32,
                SO_DISK => p.rd + p.wr,
                _ => 0.0,
            };
            hyst_sv(&mut p.sv, &mut p.svt, v, so, now, snap);
        }
        for (i, p) in d.list.iter().enumerate() {
            if p.gone != 0 {
                continue;
            }
            threads += p.threads as u32;
            cnt[0] += 1;
            cnt[match p.state {
                b'R' => 1,
                b'S' => 2,
                b'D' => 3,
                b'T' | b't' => 4,
                b'Z' => 5,
                _ => 6,
            }] += 1;
            let mut f = F::new();
            f.int(p.pid, 0);
            vis[i] = (filt.is_empty()
                || lower_has(p.name(), &filt)
                || lower_has(p.cmd(), &filt)
                || lower_has(d.user(p.uid), &filt)
                || digits && f.s().starts_with(&filt)) as u8;
        }
        let list = &d.list;
        self.pv.cnt = cnt;
        self.pv.threads = threads;
        self.pv.shown = vis.iter().filter(|&&v| v == 1).count() as u32;
        let parent = |i: usize| -> Option<usize> {
            let p = list.get(i)?;
            d.find(p.ppid).filter(|_| p.ppid != p.pid)
        };
        // tree: the ancestors of matches too
        if self.pv.tree && !filt.is_empty() {
            for i in 0..n {
                let mut j = i;
                for _ in 0..64 {
                    match (vis.get(j) != Some(&0)).then(|| parent(j)).flatten() {
                        Some(k) if vis.get(k) == Some(&0) => {
                            vis[k] = 2;
                            j = k;
                        }
                        _ => break,
                    }
                }
            }
        }
        // sort keys: (value with the direction applied) << 32 | index; names compare
        // case-insensitively
        let key32 = |sv: f32, pid: u32| -> u64 {
            let q = match so {
                SO_CPU => (sv * 10000.0) as u32,
                SO_MEM => (sv / 4096.0) as u32,
                SO_DISK => sv.min(4e9) as u32,
                _ => pid,
            };
            (if (so <= SO_DISK) != rev { !q } else { q } as u64) << 32
        };
        let by_name = (so == SO_NAME).then_some((&list[..], rev));
        let mut order: Vec<u64> = (0..n)
            .filter(|&i| vis[i] != 0)
            .filter_map(|i| list.get(i).map(|p| key32(p.sv, p.pid) | i as u64))
            .collect();
        hsort(&mut order, by_name);
        let idx = |k: u64| (k & 0xffff_ffff) as u32;
        let mut rows: Vec<Row> = Vec::new();
        let mut aggs: Vec<Agg> = Vec::new();
        if self.pv.group && !self.pv.tree {
            // programs: (name hash, place in the sorted order), so each group's
            // members come together, in order
            let mut by: Vec<u64> = order
                .iter()
                .enumerate()
                .map(|(r, &k)| {
                    let h = list
                        .get(idx(k) as usize)
                        .map_or(0, |p| procs_hash(p.group()));
                    (h as u64) << 32 | r as u64
                })
                .collect();
            hsort(&mut by, None);
            let old = std::mem::take(&mut self.pv.aggs);
            for &b in &by {
                let h = (b >> 32) as u32;
                let Some(&k) = order.get(idx(b) as usize) else {
                    continue;
                };
                let Some(p) = list.get(idx(k) as usize) else {
                    continue;
                };
                if aggs.last().is_none_or(|a| a.key != h) {
                    aggs.push(Agg {
                        key: h,
                        first: idx(k),
                        rank: u32::MAX,
                        prank: old.iter().find(|o| o.key == h).map_or(u32::MAX, |o| o.rank),
                        ..Default::default()
                    });
                }
                let Some(a) = aggs.last_mut() else { continue };
                a.mem.push(idx(k));
                a.n += 1;
                a.cpu += p.cpu;
                a.rss += p.rss;
                a.rd += p.rd;
                a.wr += p.wr;
                a.thr += p.threads as u32;
                a.noio |= p.flags & NOIO != 0;
                a.iok |= p.flags & IOK != 0;
            }
            // each group's own sort value (with hysteresis: from its last one)
            for a in aggs.iter_mut() {
                let v = match so {
                    SO_CPU => a.cpu,
                    SO_MEM => a.rss as f32,
                    _ => a.rd + a.wr,
                };
                let (mut sv, mut svt) = old
                    .iter()
                    .find(|o| o.key == a.key)
                    .map_or((v, 0), |o| (o.sv, o.svt));
                hyst_sv(&mut sv, &mut svt, v, so, now, snap);
                (a.sv, a.svt) = (sv, svt);
            }
            // groups by their summed sort values (pid / name: their first member's)
            let mut gord: Vec<u64> = aggs
                .iter()
                .enumerate()
                .map(|(gi, a)| {
                    let pid = list.get(a.first as usize).map_or(0, |p| p.pid);
                    let k = if so == SO_NAME {
                        a.first as u64
                    } else {
                        gi as u64
                    };
                    key32(if so >= SO_PID { 0.0 } else { a.sv }, pid) | k
                })
                .collect();
            hsort(&mut gord, by_name);
            for &k in &gord {
                let gi = if so == SO_NAME {
                    aggs.iter().position(|a| a.first == idx(k))
                } else {
                    Some(idx(k) as usize)
                };
                let Some(a) = gi.and_then(|gi| aggs.get(gi).map(|a| (gi, a))) else {
                    continue;
                };
                let (gi, a) = a;
                let open = a.n > 1 && self.pv.folds.contains(&((1u64 << 63) | a.key as u64));
                rows.push(Row {
                    idx: a.first,
                    g: if a.n > 1 { gi as u32 + 1 } else { 0 },
                    fl: if a.n > 1 { R_KIDS } else { 0 } | if open { R_OPEN } else { 0 },
                    ..Default::default()
                });
                if open {
                    for (m, &i) in a.mem.iter().enumerate() {
                        rows.push(Row {
                            idx: i,
                            depth: 1,
                            fl: if m + 1 == a.mem.len() { R_LAST } else { 0 },
                            ..Default::default()
                        });
                    }
                }
            }
        } else if self.pv.tree {
            // children under their parents; each list is built backwards from the
            // sorted order, so it holds the siblings last to first
            const NONE: u32 = u32::MAX;
            let mut first = vec![NONE; n];
            let mut next = vec![NONE; n];
            let mut stack: Vec<u64> = Vec::new();
            for &k in &order {
                let i = idx(k) as usize;
                match parent(i).filter(|&j| vis.get(j).is_some_and(|&v| v != 0)) {
                    Some(j) => {
                        next[i] = first[j];
                        first[j] = i as u32;
                    }
                    None => stack.push(i as u64),
                }
            }
            stack.reverse();
            // depth-first: (index, depth << 32, guides << 40, last << 56)
            while let Some(e) = stack.pop() {
                let (i, dep, guide, last) =
                    (idx(e), (e >> 32) as u8, (e >> 40) as u16, e >> 56 & 1 == 1);
                let kids = first.get(i as usize).is_some_and(|&f| f != NONE);
                let folded = list
                    .get(i as usize)
                    .is_some_and(|p| self.pv.folds.contains(&p.key()));
                rows.push(Row {
                    idx: i,
                    g: 0,
                    depth: dep,
                    fl: if kids { R_KIDS } else { 0 }
                        | if kids && !folded { R_OPEN } else { 0 }
                        | if last { R_LAST } else { 0 },
                    guide,
                });
                if kids && !folded && dep < 60 {
                    let g = if dep >= 1 && !last && dep < 16 {
                        guide | 1 << dep
                    } else {
                        guide
                    };
                    // pushed last to first: the first sibling comes off first
                    let mut x = first[i as usize];
                    let mut lst = 1u64;
                    while x != NONE {
                        stack.push(
                            x as u64 | ((dep + 1) as u64) << 32 | (g as u64) << 40 | lst << 56,
                        );
                        lst = 0;
                        x = next.get(x as usize).copied().unwrap_or(NONE);
                    }
                }
            }
        } else {
            rows.extend(order.iter().map(|&k| Row {
                idx: idx(k),
                ..Default::default()
            }));
        }
        drop(order);
        drop(vis);
        // remember each row's place (where a later slide starts)
        for p in d.list.iter_mut() {
            p.prank = p.rank;
            p.rank = u32::MAX;
            if p.prank != u32::MAX {
                p.flags &= !NEW; // was shown already: no fade-in
            }
        }
        for (k, r) in rows.iter().enumerate() {
            if r.g > 0 {
                if let Some(a) = aggs.get_mut(r.g as usize - 1) {
                    a.rank = k as u32;
                }
            } else if let Some(p) = d.list.get_mut(r.idx as usize) {
                p.rank = k as u32;
            }
        }
        self.pv.d = Some(d);
        self.pv.rows = rows;
        self.pv.aggs = aggs;
        // selection: stays on its row, so a new order never moves the table; with
        // `f` (follow) it stays on its process, which may then leave the screen (the
        // table still doesn't move; Home or Esc come back)
        let nrows = self.pv.rows.len();
        let found = if self.pv.pinned {
            self.pfind(self.pv.sel)
        } else {
            None
        };
        if self.pv.pinned && found.is_none() {
            self.pv.pinned = false; // the followed process is gone
        }
        self.pv.sel_i = found.unwrap_or(self.pv.sel_i.min(nrows.saturating_sub(1)));
        if let Some(r) = self.pv.rows.get(self.pv.sel_i).copied() {
            self.pv.sel = self.row_key(&r);
        }
        let vh = self.prows_vis();
        self.pv.scroll = self.pv.scroll.min(nrows.saturating_sub(vh.max(1)));
        if snap && !self.pv.pinned {
            self.pfollow();
        }
        self.psync(snap);
        if self.pv.det.pid == 0 || self.pv.det.start != self.psel_proc().map_or(0, |p| p.1) {
            self.pdetail(false);
        }
    }

    /// A row's group (group rows).
    fn pagg(&self, r: &Row) -> Option<&Agg> {
        self.pv.aggs.get((r.g as usize).wrapping_sub(1))
    }

    /// The index of the row with `key`.
    fn pfind(&self, key: u64) -> Option<usize> {
        (0..self.pv.rows.len())
            .find(|&i| self.pv.rows.get(i).is_some_and(|r| self.row_key(r) == key))
    }

    /// Keep the selection on screen.
    fn pfollow(&mut self) {
        let vh = self.prows_vis().max(1);
        let s = &mut self.pv;
        if s.sel_i < s.scroll {
            s.scroll = s.sel_i;
        } else if s.sel_i >= s.scroll + vh {
            s.scroll = s.sel_i + 1 - vh;
        }
    }

    /// (pid, start) of the selected process (a group: its first member).
    fn psel_proc(&self) -> Option<(u32, u64)> {
        let r = self.pv.rows.get(self.pv.sel_i)?;
        let p = self.prow_proc(r)?;
        Some((p.pid, p.start))
    }

    /// Read the selected process's details (`keep`: only if it is another process).
    fn pdetail(&mut self, refresh: bool) {
        let Some((pid, start)) = self.psel_proc() else {
            return;
        };
        let now = crate::term::mono();
        if !refresh && self.pv.det.pid == pid && self.pv.det.start == start {
            return;
        }
        if let Some(d) = self.pv.d.as_mut() {
            self.pv.det = d.detail(pid, start, now);
        }
    }

    /// Values a row shows: (CPU % of one thread, resident bytes).
    fn row_vals(&self, r: &Row) -> (f32, f32) {
        if r.g > 0 {
            self.pagg(r)
                .map_or((0.0, 0.0), |a| (a.cpu * 100.0, a.rss as f32))
        } else {
            self.prow_proc(r)
                .map_or((0.0, 0.0), |p| (p.cpu * 100.0, p.rss as f32))
        }
    }

    /// Match the on-screen rows (animation states) to the rows in the window: new
    /// targets; rows leaving slide out, exited processes fade. `snap`: no motion.
    pub fn psync(&mut self, snap: bool) {
        self.pv.ver += 1;
        let smooth = self.smooth() && !snap;
        let vh = self.prows_vis();
        let (s0, s1) = (
            self.pv.scroll,
            (self.pv.scroll + vh).min(self.pv.rows.len()),
        );
        for a in self.pv.ra.iter_mut() {
            a.live = false;
            a.rest = false;
        }
        let mut want: Vec<(u32, u64)> = Vec::new();
        for i in s0..s1 {
            let Some(&r) = self.pv.rows.get(i) else {
                break;
            };
            let key = self.row_key(&r);
            let (c, m) = self.row_vals(&r);
            let (pid, start, new, mut from) = match self.prow_proc(&r) {
                Some(p) => (p.pid, p.start, p.flags & NEW != 0, p.prank),
                None => (0, 0, false, u32::MAX),
            };
            if let Some(a) = self.pagg(&r) {
                from = a.prank;
            }
            if let Some(a) = self.pv.ra.iter_mut().find(|a| a.key == key) {
                a.live = true;
                a.gone = false;
                a.row = r;
                a.target = i as f32;
                // (smooth: changes within the noise of a 1 s CPU window are ignored)
                if !smooth || c == 0.0 || (c - a.cpu.to).abs() >= (0.3f32).max(0.04 * c) {
                    a.cpu.set(c, smooth);
                }
                a.mem.set(m, smooth);
                if !smooth {
                    a.pos = a.target;
                    a.vel = 0.0;
                }
            } else {
                // entering the window: slides in from the edge it comes from (new
                // processes fade in where they are)
                let mut v = Val::default();
                v.set(c, false);
                let mut w = Val::default();
                w.set(m, false);
                let pos = if smooth && from != u32::MAX {
                    (from as f32).max(s0 as f32 - 1.0).min((s0 + vh) as f32)
                } else {
                    i as f32
                };
                self.pv.ra.push(RA {
                    key,
                    pid,
                    start,
                    row: r,
                    pos,
                    vel: 0.0,
                    target: i as f32,
                    cpu: v,
                    mem: w,
                    alpha: if smooth && new && from == u32::MAX {
                        0.0
                    } else {
                        1.0
                    },
                    live: true,
                    rest: false,
                    gone: false,
                });
            }
            // (a group's members: only the selected group's, for its history)
            if let Some(a) = self.pagg(&r).filter(|_| key == self.pv.sel) {
                for &m in a.mem.iter().take(24) {
                    if let Some(p) = self.pv.d.as_ref().and_then(|d| d.list.get(m as usize)) {
                        want.push((p.pid, p.start));
                    }
                }
            } else if pid > 0 {
                want.push((pid, start));
            }
        }
        // rows that were on screen: still in the table (moved out of the window) →
        // slide out from where they are; gone → fade where they are
        let rows_len = self.pv.rows.len();
        let (lo, hi) = (s0 as f32 - 1.0, (s0 + vh) as f32);
        let (d, aggs) = (self.pv.d.as_ref(), &self.pv.aggs);
        self.pv.ra.retain_mut(|a| {
            if a.live || !smooth {
                return a.live;
            }
            let rank = if a.key >> 63 == 1 {
                agg_of(aggs, a.key).map(|g| g.rank)
            } else {
                d.and_then(|d| d.get(a.pid, a.start))
                    .filter(|p| p.gone == 0)
                    .map(|p| p.rank)
            };
            match rank {
                Some(r) if (r as usize) < rows_len => a.target = (r as f32).max(lo).min(hi),
                _ => a.gone = true,
            }
            true
        });
        if let Some(s) = self.psel_proc() {
            want.push(s);
        }
        if let Some(d) = self.pv.d.as_mut() {
            d.track(&want);
        }
    }

    /// Smooth mode: one frame of slides, digits, bars and fades. Whether anything moves.
    pub fn proc_step(&mut self, dt: f32) -> bool {
        if self.page != PG_PROC {
            return false;
        }
        let xs = (-W_SLIDE * dt).exp();
        let xl = (-W_LOAD * dt).exp();
        let mut mv = false;
        let vh = self.prows_vis() as f32;
        let s0 = self.pv.scroll as f32;
        let ncpu = self.s.f.len().max(1) as f32;
        let total = self.pv.total;
        for a in self.pv.ra.iter_mut().filter(|a| !a.rest) {
            let mut m = spring(&mut a.pos, &mut a.vel, a.target, dt, W_SLIDE, xs, 0.02);
            m |= a.cpu.step(dt, W_LOAD, xl, 0.01);
            let u = if total {
                0.1 * ncpu
            } else if a.cpu.x >= 99.5 {
                1.0
            } else {
                0.1
            };
            m |= a.cpu.digits(dt, u, 100.0);
            m |= a.mem.step(dt, W_LOAD, xl, 1.0);
            m |= a.mem.digits(dt, bstep(a.mem.x), 0.0);
            let fade = if a.gone { -dt / FADE_OUT } else { dt / FADE_IN };
            let al = (a.alpha + fade).clamp(0.0, 1.0);
            m |= al != a.alpha;
            a.alpha = al;
            // (settled: skipped until new targets)
            a.rest = !m;
            mv |= m;
        }
        // drop what has faded out, or slid out of the window
        self.pv.ra.retain(|a| {
            a.live || (!a.gone && a.pos > s0 - 1.0 && a.pos < s0 + vh) || (a.gone && a.alpha > 0.0)
        });
        mv
    }

    // ── layout ─────────────────────────────────────────────────────────────

    /// Table over the detail strip in `area`, and the columns that fit.
    pub fn proc_layout(&mut self, area: Rect) {
        let h = area.h;
        let dh = if h >= 26 {
            (h * 28 / 100).clamp(9, 12)
        } else if h >= 16 {
            5
        } else {
            0
        };
        let mut l = PLay {
            table: Rect { h: h - dh, ..area },
            detail: if dh > 0 {
                Rect {
                    y: area.y + h - dh,
                    h: dh,
                    ..area
                }
            } else {
                Rect::default()
            },
            ..Default::default()
        };
        let iw = l.table.inner().w;
        let mut on = [true; NC];
        let bw = |iw: usize| (iw / 14).clamp(8, 14);
        let need = |on: &[bool; NC]| -> usize {
            (0..NC)
                .filter(|&c| on[c])
                .map(|c| if c == C_BAR { bw(iw) + 1 } else { CW[c] })
                .sum::<usize>()
                + CMD_MIN
        };
        for &c in DROP.iter() {
            if need(&on) <= iw {
                break;
            }
            on[c] = false;
        }
        let mut x = 0;
        for c in 0..NC {
            l.cx[c] = usize::MAX;
            if on[c] {
                l.cx[c] = x;
                x += if c == C_BAR { bw(iw) + 1 } else { CW[c] };
            }
        }
        l.bw = if on[C_BAR] { bw(iw) } else { 0 };
        l.cmd_x = x + 1;
        self.pv.lay = l;
        self.pv.memo.clear();
    }

    /// Panels the page draws (kitty glass placements).
    pub fn page_rects(&self) -> [Rect; 8] {
        let mut r = [Rect::default(); 8];
        match self.page {
            PG_OVERVIEW if !self.ov.lay.lines => {
                r[..7].copy_from_slice(&self.ov.lay.rects());
                r[7] = self.ov.lay.strip;
            }
            PG_PROC => (r[0], r[1]) = (self.pv.lay.table, self.pv.lay.detail),
            PG_NET => r[..4].copy_from_slice(&self.nl.rects()),
            PG_SENS => r[..5].copy_from_slice(&self.zl.rects()),
            _ => {}
        }
        r
    }

    // ── keys ───────────────────────────────────────────────────────────────

    fn pmsg(&mut self, s: String, bad: bool) {
        self.pv.msg = s;
        self.pv.msg_bad = bad;
        self.pv.msg_until = crate::term::mono() + 6_000_000_000;
    }

    /// The processes page's keys (and its dialogs / filter line, which take every
    /// key). Returns false for keys it leaves to the global handler.
    pub fn proc_key(&mut self, k: Key) -> bool {
        if self.page != PG_PROC {
            return false;
        }
        if self.pv.dlg.is_some() {
            self.pdlg_key(k);
            return true;
        }
        if self.pv.editing {
            match k {
                Key::Esc => {
                    self.pv.filter.clear();
                    self.pv.editing = false;
                }
                Key::Enter | Key::Down | Key::Up => self.pv.editing = false,
                Key::Backspace => {
                    self.pv.filter.pop();
                }
                Key::CtrlU => self.pv.filter.clear(),
                Key::Char(c) if !c.is_control() && self.pv.filter.len() < 60 => {
                    self.pv.filter.push(c)
                }
                Key::CtrlC => return false,
                _ => {}
            }
            self.pv.scroll = 0;
            self.prebuild(true);
            return true;
        }
        let n = self.pv.rows.len();
        let vh = self.prows_vis().max(1);
        let i = self.pv.sel_i;
        // a followed process off screen: arrows start again from the rows shown
        let i = if i < self.pv.scroll || i >= self.pv.scroll + vh {
            self.pv.scroll
        } else {
            i
        };
        let to = match k {
            Key::Up => i.saturating_sub(1),
            Key::Down => i + 1,
            Key::PgUp => i.saturating_sub(vh - 1),
            Key::PgDn => i + vh - 1,
            Key::Home => 0,
            Key::End => n,
            _ => usize::MAX,
        };
        if to != usize::MAX {
            self.psel(to, false);
            return true;
        }
        match k {
            Key::Char('f') => {
                // follow the selected process (or stop following)
                self.pv.pinned = !self.pv.pinned;
                if !self.pv.pinned {
                    let s = self.pv.scroll;
                    self.pv.sel_i = self.pv.sel_i.clamp(s, (s + vh).min(n).max(s + 1) - 1);
                    self.psel(self.pv.sel_i, false);
                }
            }
            Key::Char('o') => {
                self.pv.sort = (self.pv.sort + 1) % NSORT;
                self.pv.rev = false;
                self.psort_changed();
            }
            Key::Char('r') => {
                self.pv.rev = !self.pv.rev;
                self.psort_changed();
            }
            Key::Char('/') => {
                self.pv.editing = true;
            }
            Key::Esc => {
                if self.pv.filter.is_empty() {
                    // back to the top, not following anything
                    self.psel(0, false);
                    return true;
                }
                self.pv.filter.clear();
                self.prebuild(true);
            }
            Key::Char('T') => {
                self.pv.tree = !self.pv.tree;
                self.pv.group = false;
                self.pv.folds.clear();
                self.prebuild(true);
            }
            Key::Char('G') => {
                self.pv.group = !self.pv.group;
                self.pv.tree = false;
                self.pv.folds.clear();
                self.prebuild(true);
            }
            Key::Char('I') => {
                self.pv.total = !self.pv.total;
                self.psync(true);
            }
            Key::Char('K') => {
                if let Some(d) = self.pv.d.as_mut() {
                    let on = !d.show_k;
                    d.set_show_k(on);
                    d.scan(crate::term::mono(), 0);
                }
                self.prebuild(true);
            }
            Key::Enter | Key::Left | Key::Right => self.pfold(k),
            Key::Char('k') | Key::F9 => self.pdlg_open(0),
            Key::Char('n') => self.pdlg_open(1),
            _ => return false,
        }
        true
    }

    /// Select row `to` (clamped), keep it on screen, read its details. `follow`: keep
    /// following its process when the order changes.
    fn psel(&mut self, to: usize, follow: bool) {
        self.pv.sel_i = to.min(self.pv.rows.len().saturating_sub(1));
        self.pv.pinned = follow;
        if let Some(r) = self.pv.rows.get(self.pv.sel_i).copied() {
            self.pv.sel = self.row_key(&r);
        }
        let s = self.pv.scroll;
        self.pfollow();
        self.psync(self.pv.scroll != s);
        self.pdetail(false);
    }

    fn psort_changed(&mut self) {
        // new sort values right away (no hysteresis across a change of column)
        if let Some(d) = self.pv.d.as_mut() {
            let want = self.pv.sort == SO_DISK;
            if want && !d.io_all {
                d.io_all = true;
                d.scan(crate::term::mono(), 0);
            }
            d.io_all = want;
        }
        self.prebuild(true);
    }

    /// Enter toggles, → opens, ← closes the selected branch / group (← on a child:
    /// to its parent row).
    fn pfold(&mut self, k: Key) {
        let Some(r) = self.pv.rows.get(self.pv.sel_i).copied() else {
            return;
        };
        if r.fl & R_KIDS == 0 {
            if k == Key::Left && r.depth > 0 {
                let i = (0..self.pv.sel_i)
                    .rev()
                    .find(|&j| self.pv.rows.get(j).is_some_and(|q| q.depth < r.depth))
                    .unwrap_or(0);
                self.psel(i, false);
            }
            return;
        }
        let key = self.row_key(&r);
        let open = r.fl & R_OPEN != 0;
        let want = match k {
            Key::Left => false,
            Key::Right => true,
            _ => !open,
        };
        if want == open {
            return;
        }
        // tree: folds hold collapsed nodes; groups: expanded ones
        let in_folds = if self.pv.tree { !want } else { want };
        self.pv.folds.retain(|&f| f != key);
        if in_folds {
            self.pv.folds.push(key);
        }
        self.prebuild(true);
    }

    fn pdlg_open(&mut self, kind: u8) {
        let Some(r) = self.pv.rows.get(self.pv.sel_i).copied() else {
            return;
        };
        let Some(d) = self.pv.d.as_ref() else { return };
        let mut who = Vec::new();
        let (label, nice) = if r.g > 0 {
            let Some(a) = self.pagg(&r) else { return };
            for p in a.mem.iter().filter_map(|&m| d.list.get(m as usize)) {
                who.push((p.pid, p.start));
            }
            let Some(p) = d.list.get(a.first as usize) else {
                return;
            };
            (
                nf("", a.n, " processes named ").s().to_string() + p.group(),
                p.nice as i32,
            )
        } else {
            let Some(p) = d.list.get(r.idx as usize) else {
                return;
            };
            who.push((p.pid, p.start));
            (
                p.name().to_string() + nf(" (pid ", p.pid, ", user ").s() + d.user(p.uid) + ")",
                p.nice as i32,
            )
        };
        self.pv.dlg = Some(Dlg {
            kind,
            sig: 0,
            nice: (nice + 5).min(19),
            old_nice: nice,
            who,
            label,
        });
    }

    fn pdlg_key(&mut self, k: Key) {
        let Some(g) = self.pv.dlg.as_mut() else {
            return;
        };
        let ns = procs::NSIG;
        match k {
            Key::Esc | Key::Char('q') | Key::CtrlC => self.pv.dlg = None,
            Key::Left | Key::Up | Key::Char('-') => {
                if g.kind == 0 {
                    g.sig = (g.sig + ns - 1) % ns;
                } else {
                    g.nice = (g.nice - 1).max(-20);
                }
            }
            Key::Right | Key::Down | Key::Char('+') => {
                if g.kind == 0 {
                    g.sig = (g.sig + 1) % ns;
                } else {
                    g.nice = (g.nice + 1).min(19);
                }
            }
            Key::Enter | Key::Char('y') => {
                let g = self.pv.dlg.take().unwrap();
                self.pact(&g);
            }
            _ => {}
        }
    }

    /// Carry out a confirmed action; the result (or why it failed) shows for a while.
    fn pact(&mut self, g: &Dlg) {
        let Some(d) = self.pv.d.as_mut() else { return };
        let (mut ok, mut bad, mut err) = (0, 0, 0);
        for &(pid, start) in &g.who {
            let r = if g.kind == 0 {
                d.signal(pid, start, procs::sig(g.sig).1)
            } else {
                d.renice(pid, start, g.nice)
            };
            match r {
                Ok(()) => ok += 1,
                Err(e) => {
                    bad += 1;
                    err = e;
                }
            }
        }
        let mut m = if ok == 0 {
            "could not act on ".to_string()
        } else if g.kind == 0 {
            procs::sig(g.sig).0.to_string() + " sent to "
        } else {
            "nice ".to_string() + fi(g.nice).s() + " set for "
        };
        m += &g.label;
        if bad > 0 {
            m += nf(": ", bad, " failed: ").s();
            m += if g.kind == 1 && err == procs::EACCES {
                "only root can raise a priority (lower nice)"
            } else {
                procs::why(err)
            };
        }
        self.pmsg(m, bad > 0);
        // fresh numbers for what changed
        let now = crate::term::mono();
        if let Some(d) = self.pv.d.as_mut() {
            d.scan(now, if self.speed == SMOOTH { 1500 } else { 0 });
        }
        self.pv.last_scan = now;
        self.prebuild(false);
        self.pdetail(true);
    }

    /// Mouse: a click selects a row (a click on the column titles: next sort); the wheel
    /// moves the selection. Returns whether it was on the page.
    pub fn proc_click(&mut self, x: usize, y: usize, btn: u16) -> bool {
        if self.page != PG_PROC || self.pv.dlg.is_some() {
            return false;
        }
        let ir = self.pv.lay.table.inner();
        if !ir.contains(x, y) && btn == 0 {
            return false;
        }
        match btn {
            // the wheel scrolls the table (3 rows); the selection keeps its screen row
            64 | 65 => {
                let vh = self.prows_vis().max(1);
                let max = self.pv.rows.len().saturating_sub(vh);
                let s = self.pv.scroll;
                let ns = if btn == 64 {
                    s.saturating_sub(3)
                } else {
                    (s + 3).min(max)
                };
                if ns != s {
                    let off = self.pv.sel_i.saturating_sub(s).min(vh - 1);
                    self.pv.scroll = ns;
                    self.psel(ns + off, false);
                }
                return true;
            }
            0 => {}
            _ => return false,
        }
        if y == ir.y + 1 {
            // the column titles: the next sort column, as `o`
            return self.proc_key(Key::Char('o'));
        }
        if y >= ir.y + 2 {
            let s = y - ir.y - 2;
            if let Some(&key) = self.pv.slots.get(s) {
                if let Some(i) = self.pfind(key) {
                    if i == self.pv.sel_i {
                        self.pfold(Key::Enter);
                    } else {
                        self.psel(i, false);
                    }
                }
            }
        }
        true
    }

    // ── drawing ────────────────────────────────────────────────────────────

    /// The page. `rows_only`: smooth in-between frames (just the table rows).
    pub fn draw_proc_page(&mut self, scr: &mut Screen, full: bool, rows_only: bool) {
        if full {
            self.pv.memo.clear();
        }
        if self.pv.d.is_none() {
            return;
        }
        let l = self.pv.lay;
        if full {
            self.ptable_frame(scr);
        }
        if !rows_only {
            self.psummary(scr);
            self.pheader(scr);
            if l.detail.h > 0 {
                self.pdetail_draw(scr, full);
            }
        }
        self.prows(scr);
        if self.pv.dlg.is_some() {
            self.pdlg_draw(scr);
        }
    }

    fn ptable_frame(&self, scr: &mut Screen) {
        let t = &self.t;
        let l = self.pv.lay;
        let so = self.pv.sort;
        let mut st = fs(" · sorted by ");
        st.str(psort_name(so));
        st.str(if self.pv.rev != (so >= SO_PID) {
            " ▲ (o: next column)"
        } else {
            " ▼ (o: next column)"
        });
        let mode = if self.pv.tree {
            " · tree"
        } else if self.pv.group {
            " · grouped by program"
        } else {
            ""
        };
        let title: [(&str, u32); 3] = [
            ("processes", t.title | BOLD),
            (st.s(), t.mid),
            (mode, t.accent2),
        ];
        let ncpu = self.s.f.len();
        let unit = if self.pv.total {
            nf(
                "CPU % of all ",
                ncpu as u32,
                " threads · memory resident · disk per second, ? not permitted",
            )
        } else {
            fs("CPU % of one CPU thread · memory resident · disk per second, ? not permitted")
        };
        let iw = l.table.inner().w;
        let sub = if iw >= unit.n + 4 {
            unit.s()
        } else if iw >= 24 {
            if self.pv.total {
                "CPU % of all"
            } else {
                "CPU % of 1 thread"
            }
        } else {
            ""
        };
        self.panel(scr, l.table, &title, sub);
        if l.detail.h > 0 {
            self.panel(
                scr,
                l.detail,
                &[("selected", t.title | BOLD)],
                if l.detail.w >= 70 {
                    if self.pv.pinned {
                        "following · f stop · enter/←→ fold · k signal · n nice"
                    } else {
                        "f follow · enter/←→ fold · k signal · n nice"
                    }
                } else {
                    ""
                },
            );
        }
    }

    /// Counts, load average, filter line, and for a while the result of an action.
    fn psummary(&mut self, scr: &mut Screen) {
        let ir = self.pv.lay.table.inner();
        if ir.h == 0 {
            return;
        }
        let msg = crate::term::mono() < self.pv.msg_until;
        let Some(d) = self.pv.d.as_ref() else { return };
        let c = self.pv.cnt;
        let k = mixk(&[
            ir.w as u64,
            c.iter().fold(0u64, |a, &c| a * 31 + c as u64),
            self.pv.threads as u64 | (d.nk as u64) << 32,
            (d.load[0] * 100.0) as u64
                | ((d.load[1] * 100.0) as u64) << 21
                | ((d.load[2] * 100.0) as u64) << 42,
            msg as u64 | (self.pv.editing as u64) << 1 | (d.show_k as u64) << 2,
            procs_hash(&self.pv.filter) as u64 | (self.pv.shown as u64) << 32,
            d.denied as u64,
        ]);
        if !pmemo(&mut self.pv.memo, K_SUM, k) {
            return;
        }
        let t = &self.t;
        let (x0, y, maxx) = (ir.x, ir.y, ir.x + ir.w);
        let line =
            |scr: &mut Screen, sp: &[(&str, u32)]| Self::line(scr, x0, y, maxx, surf(S_PANEL), sp);
        if self.pv.editing {
            line(
                scr,
                &[
                    ("filter: ", t.accent | BOLD),
                    (&self.pv.filter, t.fg | BOLD),
                    (
                        "▏  enter keep · esc clear · matches name, command, user, pid",
                        t.dim,
                    ),
                ],
            );
            return;
        }
        if msg {
            let m = &self.pv.msg;
            line(
                scr,
                &[(
                    m,
                    if self.pv.msg_bad {
                        t.crit | BOLD
                    } else {
                        t.good | BOLD
                    },
                )],
            );
            return;
        }
        let mut rest = nf(" processes: ", c[1], " running · ");
        fnum(&mut rest, "", c[2] + c[6], " sleeping");
        if c[3] > 0 {
            fnum(&mut rest, " · ", c[3], " disk wait");
        }
        if c[4] > 0 {
            fnum(&mut rest, " · ", c[4], " stopped");
        }
        fnum(&mut rest, " · ", c[5], " zombie");
        // load average with two decimals (fixed point: no float formatting)
        let mut ld = nf("  ", self.pv.threads, " threads  load");
        for v in d.load {
            let c = (v.max(0.0) * 100.0).round() as u32;
            fnum(&mut ld, " ", c / 100, ".");
            ld.push(b'0' + (c / 10 % 10) as u8);
            ld.push(b'0' + (c % 10) as u8);
        }
        let kt = nf(
            "  ",
            d.nk,
            if d.show_k {
                " kernel threads (K)"
            } else {
                " kernel threads hidden (K)"
            },
        );
        let mut filt = F::new();
        if !self.pv.filter.is_empty() {
            filt.str("  filter \"");
            filt.str(&self.pv.filter);
            fnum(&mut filt, "\": ", self.pv.shown, " match");
        }
        let others = d.list.iter().any(|p| p.uid != d.me);
        let note = if d.me != 0 && d.hide >= 2 && !others {
            "  only your processes are visible (/proc hidepid)"
        } else if d.denied > 0 {
            "  some processes are not readable (/proc hidepid)"
        } else {
            ""
        };
        line(
            scr,
            &[
                (nf("", c[0], "").s(), t.fg | BOLD),
                (rest.s(), t.mid),
                (ld.s(), t.text),
                (filt.s(), t.accent | BOLD),
                (note, t.warn),
                (kt.s(), t.dim),
            ],
        );
    }

    /// Column titles: the sorted one highlighted with its direction.
    fn pheader(&mut self, scr: &mut Screen) {
        let ir = self.pv.lay.table.inner();
        if ir.h < 2 {
            return;
        }
        let l = self.pv.lay;
        let k = mixk(&[
            ir.w as u64,
            self.pv.sort as u64,
            self.pv.rev as u64,
            self.pv.total as u64,
            l.bw as u64,
            self.pv.tree as u64,
            self.pv.group as u64,
        ]);
        if !pmemo(&mut self.pv.memo, K_HEAD, k) {
            return;
        }
        let t = &self.t;
        let bg = surf(S_PANEL);
        let (x0, y, maxx) = (ir.x, ir.y + 1, ir.x + ir.w);
        Self::line(scr, x0, y, maxx, bg, &[]);
        let so = self.pv.sort;
        let arrow = if self.pv.rev != (so >= SO_PID) {
            "▲"
        } else {
            "▼"
        };
        let ncpu = self.s.f.len();
        let of = if self.pv.total {
            nf("of all ", ncpu as u32, "")
        } else {
            fs("of 1 CPU")
        };
        // (one string: a table of &str costs a relocation each)
        for (c, lab) in "PID USER CPU% - MEMORY RAM% READ/s WRIT/s THR S AGE"
            .split(' ')
            .enumerate()
        {
            if c >= NC || l.cx[c] == usize::MAX || c == C_BAR {
                continue;
            }
            let hi = so
                == match c {
                    C_PID => SO_PID,
                    C_CPU => SO_CPU,
                    C_MEM | C_RAM => SO_MEM,
                    C_RD | C_WR => SO_DISK,
                    _ => 99,
                };
            let w = CW[c];
            let left = c == C_USER || c == C_ST;
            let mut lab = fs(lab);
            if hi && c != C_RAM && c != C_WR {
                lab.str(arrow);
            }
            let n = lab.s().chars().count();
            let x = if left {
                x0 + l.cx[c] + 1
            } else {
                x0 + l.cx[c] + w - n.min(w)
            };
            scr.text(
                x,
                y,
                lab.s(),
                if hi { t.accent | BOLD } else { t.dim },
                bg,
                maxx,
            );
        }
        if l.bw > 0 {
            scr.text(
                x0 + l.cx[C_BAR] + 1,
                y,
                of.s(),
                t.dim,
                bg,
                x0 + l.cx[C_BAR] + 1 + l.bw,
            );
        }
        let hi = so == SO_NAME;
        let cmd = if self.pv.group && !self.pv.tree {
            "PROGRAM ×count · COMMAND"
        } else {
            "COMMAND"
        };
        let x = scr.text(
            x0 + l.cmd_x,
            y,
            cmd,
            if hi { t.accent | BOLD } else { t.dim },
            bg,
            maxx,
        );
        if hi {
            scr.text(x, y, arrow, t.accent | BOLD, bg, maxx);
        }
    }

    /// The table rows: the animation states in order of position, one per slot.
    fn prows(&mut self, scr: &mut Screen) {
        let ir = self.pv.lay.table.inner();
        let vh = self.prows_vis();
        if vh == 0 {
            return;
        }
        let s0 = self.pv.scroll as f32;
        // (position, then target: fixed point; the index in `ra`)
        let fx = |v: f32| ((v + 1e4) * 256.0) as u64;
        let mut cand = std::mem::take(&mut self.pv.cand);
        cand.clear();
        cand.extend(
            self.pv
                .ra
                .iter()
                .enumerate()
                .filter(|(_, a)| {
                    (a.live && a.target >= s0 && a.target < s0 + vh as f32)
                        || (a.pos >= s0 - 0.5 && a.pos < s0 + vh as f32 - 0.5)
                })
                .map(|(k, a)| fx(a.pos) << 40 | fx(a.target) << 12 | k as u64),
        );
        hsort(&mut cand, None);
        self.pv.slots.clear();
        let y0 = ir.y + 2;
        for s in 0..vh {
            let y = y0 + s;
            match cand
                .get(s)
                .and_then(|&c| self.pv.ra.get((c & 0xfff) as usize))
            {
                Some(&a) => {
                    self.pv.slots.push(a.key);
                    self.prow(scr, y, &a);
                }
                None => {
                    if pmemo(&mut self.pv.memo, K_ROWS + s, 1) {
                        Self::line(scr, ir.x, y, ir.x + ir.w, surf(S_PANEL), &[]);
                    }
                }
            }
        }
        self.pv.cand = cand;
        if self.pv.rows.is_empty() && vh > 0 {
            let msg = if !self.pv.filter.is_empty() {
                "no process matches the filter (esc clears it)"
            } else if self.pv.d.as_ref().is_some_and(|d| !d.ok) {
                "/proc is not readable"
            } else {
                "scanning…"
            };
            if pmemo(&mut self.pv.memo, K_ROWS, 2) {
                Self::line(
                    scr,
                    ir.x,
                    y0,
                    ir.x + ir.w,
                    surf(S_PANEL),
                    &[(msg, self.t.dim)],
                );
            }
        }
    }

    /// A row's CPU bar filled to `fill` sixteenths (faded with the row, deeper behind
    /// its head as in `meter`).
    #[allow(clippy::too_many_arguments)]
    fn pbar(&self, scr: &mut Screen, x: usize, y: usize, fill: usize, bg: u32, fb: u32, al: f32) {
        let bw = self.pv.lay.bw;
        for i in 0..bw {
            let fg = if i < fill / 16 || (i == fill / 16 && fill % 16 > 0) {
                let c = fade(self.t.load.at(i as f32 / (bw.max(2) - 1) as f32), fb, al);
                if i == fill / 16 {
                    c | ((fill % 16) as u32) << BLEND_SHIFT
                } else {
                    self.deep(c, bar_depth(i, fill))
                }
            } else {
                TRACK
            };
            scr.put(x + i, y, LOAD, fg, bg);
        }
    }

    /// One table row (skipped when nothing in it changed).
    fn prow(&mut self, scr: &mut Screen, y: usize, a: &RA) {
        let ir = self.pv.lay.table.inner();
        let slot = y - ir.y - 2;
        let l = self.pv.lay;
        let r = a.row;
        let sel = a.key == self.pv.sel;
        let div = if self.pv.total {
            self.s.f.len().max(1) as f32
        } else {
            1.0
        };
        let (cpu_n, cpu_x) = (a.cpu.n / div, a.cpu.x / div);
        let fill = fill16((cpu_x / 100.0).clamp(0.0, 1.0), l.bw);
        // what the row shows changes with its animation or the data (`ver`): checked
        // first, so most frames cost no lookups
        let key = mixk(&[
            a.key,
            sel as u64 | ((a.alpha * 32.0) as u64 * 2),
            cpu_n.to_bits() as u64,
            a.mem.n.to_bits() as u64,
            self.pv.ver,
            r.fl as u64 | (r.depth as u64) << 8 | (r.guide as u64) << 16 | (r.g as u64) << 32,
        ]);
        let (bg, fb) = if sel {
            (surf(S_SELECT), self.t.select)
        } else {
            (surf(S_PANEL), self.t.panel)
        };
        let bar = pmemo(&mut self.pv.memo, K_BARS + slot, mixk(&[fill as u64, key]));
        if !pmemo(&mut self.pv.memo, K_ROWS + slot, key) {
            // only the bar moved: just its cells
            if bar && l.bw > 0 {
                self.pbar(scr, ir.x + l.cx[C_BAR] + 1, y, fill, bg, fb, a.alpha);
            }
            return;
        }
        let Some(d) = self.pv.d.as_ref() else { return };
        let agg = agg_of(&self.pv.aggs, a.key);
        let j = match agg {
            Some(g) => Some(g.first as usize),
            None => d.find(a.pid),
        };
        let Some(p) = j
            .and_then(|j| d.list.get(j))
            .filter(|p| agg.is_some() || p.start == a.start)
        else {
            Self::line(scr, ir.x, y, ir.x + ir.w, surf(S_PANEL), &[]);
            return;
        };
        let now_s = (crate::term::now_ns(crate::term::CLOCK_BOOTTIME) / 1_000_000_000) as u64;
        let ag = age(now_s.saturating_sub((p.start as f32 / d.hz) as u64));
        let group = agg.is_some();
        let (rd, wr, noio, iok, thr, n) = match agg {
            Some(g) => (g.rd, g.wr, g.noio && !g.iok, g.iok, g.thr, g.n),
            None => (
                p.rd,
                p.wr,
                p.flags & NOIO != 0,
                p.flags & IOK != 0,
                p.threads as u32,
                1,
            ),
        };
        let t = &self.t;
        let x0 = ir.x;
        let mut w = Pen {
            scr,
            x: x0,
            y,
            maxx: x0 + ir.w,
            bg,
            fb,
            al: a.alpha,
        };
        // right-aligned value in column `col`
        let cell = |w: &mut Pen, col: usize, s: &str, fg: u32| {
            if l.cx[col] != usize::MAX {
                w.right(x0 + l.cx[col] + CW[col], s, fg);
            }
        };
        let pid = if group {
            nf("×", n, "")
        } else {
            nf("", p.pid, "")
        };
        cell(&mut w, C_PID, pid.s(), if group { t.accent } else { t.dim });
        if l.cx[C_USER] != usize::MAX {
            w.to(x0 + l.cx[C_USER] + 1);
            let u = d.user(p.uid);
            let cut = u.char_indices().nth(7).map_or(u.len(), |c| c.0);
            let long = u.chars().count() > 8;
            w.put(
                if long { &u[..cut] } else { u },
                if p.uid == 0 { t.dim } else { t.mid },
            );
            if long {
                w.put("+", t.dim);
            }
        }
        let cp = if p.flags & NORATE != 0 && !group && cpu_n == 0.0 {
            fs("·")
        } else {
            cpct(cpu_n)
        };
        let cc = if cpu_n < 0.05 {
            t.dim
        } else {
            t.load.at((cpu_n / 100.0).min(1.0)) | BOLD
        };
        cell(&mut w, C_CPU, cp.s(), cc);
        if l.bw > 0 {
            w.to(x0 + l.cx[C_BAR] + 1);
            self.pbar(w.scr, w.x, y, fill, bg, fb, a.alpha);
            w.x += l.bw;
        }
        let total_ram = (self.m.mi.total as f32).max(1.0);
        cell(&mut w, C_MEM, bsize(a.mem.n, false).s(), t.fg);
        cell(
            &mut w,
            C_RAM,
            pct1((a.mem.n / total_ram).min(1.0)).s(),
            t.mid,
        );
        for (col, v) in [(C_RD, rd), (C_WR, wr)] {
            let s = if noio {
                fs("?")
            } else if !iok {
                fs("·")
            } else if v < 1.0 {
                fs("0")
            } else {
                bsize(v, true)
            };
            cell(
                &mut w,
                col,
                s.s(),
                if v >= 1.0 && iok { t.text } else { t.dim },
            );
        }
        cell(&mut w, C_THR, nf("", thr, "").s(), t.mid);
        let (ch, sc) = match p.state {
            b'R' => ("R", t.good | BOLD),
            b'D' => ("D", t.warn | BOLD),
            b'Z' => ("Z", t.crit | BOLD),
            b'T' | b't' => ("T", t.warn),
            b'I' => ("I", t.dim),
            _ => ("S", t.dim),
        };
        cell(&mut w, C_ST, ch, sc);
        cell(&mut w, C_AGE, ag.s(), t.dim);
        // command: tree guides, fold marker, name, ×count, arguments
        w.to(x0 + l.cmd_x);
        let room = w.maxx.saturating_sub(w.x);
        let dep = r.depth as usize;
        if dep > 0 && room > 24 {
            for k in 1..dep.min((room - 16) / 2) {
                w.put(if r.guide >> k & 1 == 1 { "│ " } else { "  " }, t.border);
            }
            w.put(
                if r.fl & R_LAST != 0 {
                    "└─"
                } else {
                    "├─"
                },
                t.border,
            );
        }
        if r.fl & R_KIDS != 0 {
            w.put(if r.fl & R_OPEN != 0 { "▾ " } else { "▸ " }, t.accent);
        }
        let kt = p.kthread();
        if kt {
            w.put("[", t.dim);
        }
        w.put(
            if group { p.group() } else { p.name() },
            if sel {
                t.fg | BOLD
            } else if kt {
                t.mid
            } else {
                t.fg
            },
        );
        if kt {
            w.put("]", t.dim);
        }
        if group {
            w.put(nf(" ×", n, "").s(), t.accent | BOLD);
        }
        if p.state == b'Z' {
            w.put(" <defunct>", t.crit);
        } else if p.gone != 0 {
            w.put(" (exited)", t.dim);
        }
        let args = if group { "" } else { p.args() };
        if !args.is_empty() && w.x + 2 < w.maxx {
            w.put(" ", 0);
            let avail = w.maxx - w.x;
            let cut = args.char_indices().nth(avail - 1).map(|c| c.0);
            match cut {
                Some(b) if args.chars().count() > avail => {
                    w.put(&args[..b], t.dim);
                    w.put("…", t.dim);
                }
                _ => w.put(args, t.dim),
            }
        }
        w.to(w.maxx);
    }

    /// The selected process (or group): facts, whole command line, cwd, cgroup, and
    /// CPU / memory over the last minute.
    fn pdetail_draw(&mut self, scr: &mut Screen, full: bool) {
        let ir = self.pv.lay.detail.inner();
        let row = self.pv.rows.get(self.pv.sel_i).copied();
        let (Some(d), Some(row)) = (self.pv.d.as_ref(), row) else {
            return;
        };
        let Some(p) = d.list.get(row.idx as usize) else {
            return;
        };
        let agg = self.pv.aggs.get((row.g as usize).wrapping_sub(1));
        let det = &self.pv.det;
        let now_s = (crate::term::now_ns(crate::term::CLOCK_BOOTTIME) / 1_000_000_000) as u64;
        let age_s = now_s.saturating_sub((p.start as f32 / d.hz) as u64);
        let charts = ir.h >= 6 && ir.w >= 40;
        let tr = d.tracked(p.pid, p.start);
        let k = mixk(&[
            p.key(),
            row.g as u64,
            agg.map_or(0, |a| a.n as u64 | (a.thr as u64) << 32),
            p.state as u64 | (p.threads as u64) << 8 | (p.nice as u8 as u64) << 40,
            det.t as u64,
            ir.w as u64,
            age(age_s).s().len() as u64 + age_s / 60,
            p.ppid as u64,
            tr.map_or(0, |t| {
                t.hlen as u64 | (t.back(&t.hc, 0).to_bits() as u64) << 8
            }),
            self.pv.total as u64,
        ]);
        if !pmemo(&mut self.pv.memo, K_DET, k) && !full {
            return;
        }
        let t = &self.t;
        let bg = surf(S_PANEL);
        let (x0, maxx) = (ir.x, ir.x + ir.w);
        let mut y = ir.y;
        // facts
        let mut s = String::new();
        let num = |s: &mut String, pre: &str, v: u32| s.push_str(nf(pre, v, "").s());
        if let Some(a) = agg {
            num(&mut s, " ×", a.n);
            num(&mut s, " processes · ", a.thr);
            s += " threads · pids";
            for &m in a.mem.iter().take(40) {
                num(&mut s, " ", d.list.get(m as usize).map_or(0, |p| p.pid));
            }
        } else {
            num(&mut s, "  pid ", p.pid);
            s += " · parent ";
            if p.ppid == 0 {
                s += "none";
            } else {
                num(&mut s, "", p.ppid);
                s += " ";
                s += d
                    .find(p.ppid)
                    .and_then(|j| d.list.get(j))
                    .map_or("", |q| q.name());
            }
            s += " · user ";
            s += d.user(p.uid);
            s += " · ";
            s += state_name(p.state);
            num(&mut s, " · threads ", p.threads as u32);
            s += " · nice ";
            s += fi(p.nice as i32).s();
            s += " · started ";
            s += age(age_s).s();
            s += " ago";
            if det.st.swap != u64::MAX && det.st.swap > 0 {
                s += " · swap ";
                s += bsize(det.st.swap as f32 * 1024.0, false).s();
            }
            if det.st.nspid > 0 {
                num(&mut s, " · pid ", det.st.nspid);
                s += " inside its container";
            }
        }
        Self::line(
            scr,
            x0,
            y,
            maxx,
            bg,
            &[
                (
                    if agg.is_some() { p.group() } else { p.name() },
                    t.fg | BOLD,
                ),
                (&s, t.mid),
            ],
        );
        y += 1;
        // command line, wrapped over the rows the charts leave
        let cmd: &str = if det.pid == p.pid && det.start == p.start && !det.cmd.is_empty() {
            &det.cmd
        } else if !p.cmd().is_empty() {
            p.cmd()
        } else if p.kthread() {
            "(kernel thread: no command line)"
        } else if p.state == b'Z' {
            "(zombie: exited, waiting for its parent to collect it)"
        } else {
            "?"
        };
        let rows = if charts {
            2
        } else {
            ir.h.saturating_sub(2).max(1)
        }
        .min(ir.h - 1);
        let mut rest = cmd;
        for k in 0..rows {
            let x = scr.text(
                x0,
                y,
                if k == 0 { "command " } else { "        " },
                t.dim,
                bg,
                maxx,
            );
            let last = k + 1 == rows;
            let room = (maxx - x).saturating_sub(last as usize);
            let b = rest.char_indices().nth(room).map_or(rest.len(), |c| c.0);
            let x = scr.text(x, y, &rest[..b], t.text, bg, maxx);
            rest = &rest[b..];
            let x = if last && !rest.is_empty() {
                scr.text(x, y, "…", t.accent, bg, maxx)
            } else {
                x
            };
            Self::line(scr, x, y, maxx, bg, &[]);
            y += 1;
        }
        if y < ir.y + ir.h {
            let cg = if det.cgroup.is_empty() {
                "?"
            } else {
                &det.cgroup
            };
            let cwd = if det.pid == p.pid {
                det.cwd.as_str()
            } else {
                "?"
            };
            Self::line(
                scr,
                x0,
                y,
                maxx,
                bg,
                &[
                    ("cwd ", t.dim),
                    (cwd, t.text),
                    ("   cgroup ", t.dim),
                    (cg, t.mid),
                ],
            );
            y += 1;
        }
        if !charts {
            return;
        }
        // CPU and memory over the last minute, side by side (a group: its members'
        // sums)
        let gh = ir.y + ir.h - y - 1;
        let half = (ir.w - 2) / 2;
        let tw = (half - 8).min(procs::HN / 2);
        let pts = 2 * tw;
        let mut v = vec![0f32; 2 * pts];
        let mut have = 0;
        let mut add = |pid: u32, st: u64| {
            if let Some(tr) = d.tracked(pid, st) {
                have = have.max(tr.hlen);
                for k in 0..pts.min(tr.hlen) {
                    v[pts - 1 - k] += tr.back(&tr.hc, k);
                    v[2 * pts - 1 - k] += tr.back(&tr.hm, k);
                }
            }
        };
        match agg {
            Some(a) => a
                .mem
                .iter()
                .filter_map(|&m| d.list.get(m as usize))
                .for_each(|q| add(q.pid, q.start)),
            None => add(p.pid, p.start),
        }
        let (cpu, mem) = v.split_at_mut(pts);
        if self.pv.total {
            let n = self.s.f.len().max(1) as f32;
            cpu.iter_mut().for_each(|c| *c /= n);
        }
        let secs = nf("   last ", pts.min(procs::HN) as u32, " s … now");
        // (nothing followed yet — just selected, or not one of the followed: say so
        // rather than show a 0 the table contradicts)
        let wait = have == 0;
        // the headline numbers are the table's (a group: all its members; the chart
        // follows up to 24 of them)
        let div = if self.pv.total { self.s.f.len().max(1) as f32 } else { 1.0 };
        let (now_c, now_m) = match agg {
            Some(a) => (a.cpu * 100.0 / div, a.rss as f32),
            None => (cpu[pts - 1], mem[pts - 1]),
        };
        let wait = wait && agg.is_none();
        // (the scales hold the headline numbers too)
        let cmax = cpu.iter().copied().fold(now_c, f32::max);
        let cscale = if self.pv.total {
            100.0
        } else {
            (cmax / 100.0).ceil().max(1.0) * 100.0
        };
        let mmax = mem.iter().copied().fold(now_m.max(1.0), f32::max);
        let mscale = nice_bytes(mmax);
        let secs = if wait { fc(&["   collecting…"]) } else { secs };
        #[allow(clippy::type_complexity)]
        let charts: [(&[f32], &str, F, f32, F, &crate::color::Ramp); 2] = [
            (
                cpu,
                if self.pv.total {
                    "CPU % of all"
                } else {
                    "CPU % of 1 CPU"
                },
                if wait { F::new() } else { cpct(now_c) },
                cscale,
                cpct(cscale),
                &t.load,
            ),
            (
                mem,
                "memory",
                if wait { F::new() } else { bsize(now_m, false) },
                mscale,
                bsize(mscale, false),
                &t.freq,
            ),
        ];
        for (k, (vals, title, now, scale, top, ramp)) in charts.into_iter().enumerate() {
            let gx = x0 + k * (half + 2);
            Self::line(
                scr,
                gx,
                y,
                gx + half,
                bg,
                &[
                    (title, t.mid),
                    ("  ", 0),
                    (now.s(), t.fg | BOLD),
                    (secs.s(), t.dim),
                ],
            );
            for yy in y + 1..y + 1 + gh {
                Self::line(scr, gx, yy, (gx + half + 2).min(maxx), bg, &[]);
            }
            self.under.set(0);
            self.axis_graph(
                scr,
                gx,
                y + 1,
                (tw + 7).min(half),
                gh,
                vals,
                scale,
                ramp,
                &[(1.0, top.s()), (0.0, "0")],
            );
        }
        for yy in y..y + 1 + gh {
            Self::line(scr, x0 + 2 * half + 2, yy, maxx, bg, &[]);
        }
    }

    /// The signal / nice dialog (drawn over the table on every frame while open).
    fn pdlg_draw(&self, scr: &mut Screen) {
        let Some(g) = self.pv.dlg.as_ref() else {
            return;
        };
        let t = &self.t;
        let w = (self.w.saturating_sub(4)).min(66);
        let h = 9;
        if w < 30 || self.h < h + 2 {
            return;
        }
        let r = Rect {
            x: (self.w - w) / 2,
            y: (self.h - h) / 2,
            w,
            h,
        };
        let bg = surf(S_DIALOG);
        let title = if g.kind == 0 {
            "send a signal"
        } else {
            "change priority (nice)"
        };
        // a panel on the dialog surface
        self.panel(scr, r, &[(title, t.title | BOLD)], "");
        scr.set_bg(r.x, r.y, r.w, r.h, bg);
        let (x0, mx) = (r.x + 2, r.x + r.w - 2);
        Self::spans(
            scr,
            x0,
            r.y + 2,
            mx,
            bg,
            &[("to ", t.dim), (&g.label, t.fg | BOLD)],
        );
        if g.kind == 0 {
            let mut x = x0;
            for k in 0..procs::NSIG {
                let name = procs::sig(k).0;
                let on = k == g.sig;
                let mut s = fs(" ");
                s.str(name);
                s.push(b' ');
                x = if on {
                    scr.text(x, r.y + 4, s.s(), t.bg | BOLD, t.accent, mx)
                } else {
                    scr.text(x, r.y + 4, s.s(), t.text, bg, mx)
                };
                x += 1;
            }
            Self::spans(
                scr,
                x0,
                r.y + 5,
                mx,
                bg,
                &[
                    (procs::sig(g.sig).0, t.accent | BOLD),
                    (" ", 0),
                    (procs::sig(g.sig).2, t.mid),
                ],
            );
        } else {
            let a = fi(g.old_nice);
            let mut b2 = fs(" ");
            b2.str(fi(g.nice).s());
            b2.push(b' ');
            Self::spans(
                scr,
                x0,
                r.y + 4,
                mx,
                bg,
                &[
                    ("nice ", t.dim),
                    (a.s(), t.text),
                    ("  →  ", t.dim),
                    (b2.s(), t.bg | BOLD),
                ],
            );
            let bx = x0 + 5 + a.n + 5;
            scr.text(bx, r.y + 4, b2.s(), t.bg | BOLD, t.accent, mx);
            Self::spans(
                scr,
                x0,
                r.y + 5,
                mx,
                bg,
                &[(
                    "higher = lower priority; going lower than now needs root",
                    t.mid,
                )],
            );
        }
        Self::spans(
            scr,
            x0,
            r.y + 7,
            mx,
            bg,
            &[
                ("enter", t.accent | BOLD),
                (" confirm · ", t.dim),
                ("←→", t.accent | BOLD),
                (
                    if g.kind == 0 {
                        " choose · "
                    } else {
                        " change · "
                    },
                    t.dim,
                ),
                ("esc", t.accent | BOLD),
                (" cancel", t.dim),
            ],
        );
    }
}

/// `pre`, the number, `post` (no core::fmt: one small copy for every number shown).
#[inline(never)]
fn fnum(f: &mut F, pre: &str, v: u32, post: &str) {
    f.str(pre);
    f.int(v, 0);
    f.str(post);
}

fn fs(s: &str) -> F {
    let mut f = F::new();
    f.str(s);
    f
}

/// A signed number.
fn fi(v: i32) -> F {
    let mut f = F::new();
    if v < 0 {
        f.push(b'-');
    }
    f.int(v.unsigned_abs(), 0);
    f
}

fn nf(pre: &str, v: u32, post: &str) -> F {
    let mut f = F::new();
    fnum(&mut f, pre, v, post);
    f
}

/// Memo of drawn parts: whether `key` differs from the last one in `slot`.
fn pmemo(memo: &mut Vec<u64>, slot: usize, key: u64) -> bool {
    if memo.len() <= slot {
        memo.resize(slot + 1, u64::MAX);
    }
    let ch = memo[slot] != key;
    memo[slot] = key;
    ch
}

fn procs_hash(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, c| {
        (h ^ c as u32).wrapping_mul(0x0100_0193)
    })
}

/// A sort value with hysteresis: follows `v` at once when they differ by more than
/// the margin of sort column `so`, else only once the difference has lasted HOLD_MS.
fn hyst_sv(sv: &mut f32, svt: &mut u32, v: f32, so: usize, now: u32, snap: bool) {
    let m = v.abs().max(sv.abs());
    let margin = match so {
        // (a clock tick a second is 1 %: the noise floor)
        SO_CPU => (0.025f32).max(0.2 * m),
        SO_MEM => 0.02 * m,
        _ => (16384.0f32).max(0.25 * m),
    };
    if v == *sv {
        *svt = 0;
    } else if snap || (v - *sv).abs() > margin || (*svt != 0 && now.wrapping_sub(*svt) >= HOLD_MS) {
        *sv = v;
        *svt = 0;
    } else if *svt == 0 {
        *svt = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake /proc: what a hidepid=2 mount shows a user (only their processes), an
    /// unreadable process, a zombie, a kernel thread, odd names, a very long command
    /// line. The real scanner reads it; the page draws it.
    #[cfg(unix)]
    #[test]
    fn restricted_proc_fixture() {
        let dir = std::env::temp_dir().join(format!("cpuview-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stat = |pid: u32, name: &str, st: char, ppid: u32, flags: u64, rss: u32| {
            format!(
                "{pid} ({name}) {st} {ppid} {pid} {pid} 0 -1 {flags} 0 0 0 0 50 10 0 0 20 0 1 0 1000 9000000 {rss} 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0\n"
            )
        };
        let long = "x".repeat(10_000);
        let procs: [(u32, String, &[u8]); 5] = [
            (
                100,
                stat(100, "my (weird) name", 'S', 1, 0, 300),
                b"/usr/bin/my app\0--flag\0",
            ),
            (200, stat(200, "defunct", 'Z', 100, 0, 0), b""),
            (300, stat(300, "kworker/0:1", 'I', 2, 0x0020_0000, 0), b""),
            (400, stat(400, "long", 'R', 1, 0, 500), long.as_bytes()),
            (500, stat(500, "secret", 'S', 1, 0, 100), b"secret\0"),
        ];
        for (pid, st, cmd) in &procs {
            let d = dir.join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("stat"), st).unwrap();
            std::fs::write(d.join("cmdline"), cmd).unwrap();
        }
        std::fs::write(dir.join("loadavg"), "1.50 0.75 0.25 2/300 999\n").unwrap();
        // not readable (another user's, hidepid=1 style)
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("500/stat");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root = unsafe { libc::geteuid() } == 0;

        let mut s = Sampler::new_for_test(&[0, 1]);
        s.t.maxf = 5000.0;
        let mut a = App::new(s, crate::theme::load("default").unwrap());
        a.page = PG_PROC;
        let mut d = Procs::new(dir.to_str().unwrap());
        d.hide = 2;
        a.pv.d = Some(d);
        a.pv.group = false;
        a.first_size(128, 45);
        a.procs_enter();
        let mut scr = Screen::new(128, 45);
        a.draw(&mut scr, true);
        let text: String = (0..45)
            .map(|y| {
                (0..128)
                    .map(|x| scr.buf[y * 128 + x].ch)
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        let d = a.pv.d.as_ref().unwrap();
        // the kernel thread is hidden and counted, the unreadable one counted
        assert_eq!(d.nk, 1);
        assert_eq!(d.list.len(), if root { 4 } else { 3 }, "{text}");
        if !root {
            assert_eq!(d.denied, 1);
        }
        assert!(text.contains("only your processes are visible"), "{text}");
        assert!(text.contains("app --flag"), "{text}");
        assert!(text.contains("<defunct>"), "{text}");
        assert!(text.contains("xxxxxxxxxxxxxxxx"), "{text}");
        assert!(text.contains("load 1.50 0.75 0.25"), "{text}");
        // no io file (or not readable): disk "?", never blank
        assert!(text.contains("      ?      ?"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn heap_sort_and_formats() {
        let mut v = vec![5u64, 1, 9, 3, 3, 0, 12, 7];
        hsort(&mut v, None);
        assert_eq!(v, vec![0, 1, 3, 3, 5, 7, 9, 12]);
        let mut e: Vec<u64> = vec![];
        hsort(&mut e, None);
        assert_eq!(age(45).s(), "45s");
        assert_eq!(age(750).s(), "12m");
        assert_eq!(age(3 * 3600 + 5 * 60).s(), "3h05m");
        assert_eq!(age(4 * 86400 + 2 * 3600).s(), "4d02h");
        assert_eq!(cpct(12.34).s(), "12.3");
        assert_eq!(cpct(0.0).s(), "0.0");
        assert_eq!(cpct(2400.4).s(), "2400");
        assert!(lower_has("PostgreSQL", "postg"));
        assert!(!lower_has("bash", "zsh"));
        assert_eq!(page_index("processes"), Some(PG_PROC));
        assert_eq!(page_index("nope"), None);
    }

    #[test]
    fn sort_values_hold_near_ties() {
        let (mut sv, mut t) = (0.10f32, 0u32);
        // jitter within the margin: no change until it lasts
        hyst_sv(&mut sv, &mut t, 0.11, SO_CPU, 1000, false);
        assert_eq!(sv, 0.10);
        hyst_sv(&mut sv, &mut t, 0.104, SO_CPU, 4000, false);
        assert_eq!(sv, 0.10);
        hyst_sv(&mut sv, &mut t, 0.104, SO_CPU, 5100, false);
        assert_eq!(sv, 0.104);
        // a real change: at once
        hyst_sv(&mut sv, &mut t, 0.5, SO_CPU, 5200, false);
        assert_eq!(sv, 0.5);
        // memory: 2 % of the value
        let (mut m, mut t) = (1000.0f32, 0u32);
        hyst_sv(&mut m, &mut t, 1010.0, SO_MEM, 1, false);
        assert_eq!(m, 1000.0);
        hyst_sv(&mut m, &mut t, 1030.0, SO_MEM, 2, false);
        assert_eq!(m, 1030.0);
    }
}

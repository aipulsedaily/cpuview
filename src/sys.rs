//! CPU topology and sampling: /proc/stat, cpufreq, hwmon, all through persistent file
//! descriptors read with pread() into preallocated buffers (no allocation per sample).

// (the Linux parsers stay compiled, and tested, on Windows too)
#![cfg_attr(windows, allow(dead_code, unused_imports))]
#[cfg(unix)]
use std::ffi::CString;
use std::os::raw::c_int;

/// The Windows sources (counters and system calls instead of /proc and sysfs).
#[cfg(windows)]
#[path = "win/cpu.rs"]
pub(crate) mod win;

pub const SYS: &str = "/sys/devices/system/cpu";
pub const HIST: usize = 1024;
/// Load window. /proc/stat counts in USER_HZ (10 ms) ticks, so a short sampling
/// interval holds only a tick or two per CPU and the load quantises to 0/50/100 %.
/// Load is therefore always computed against the newest snapshot at least this old:
/// ≥ 48 ticks, ±2 % resolution, at any sampling rate.
pub const WINDOW_NS: i64 = 500_000_000;
/// /proc/stat snapshots kept for the window (enough for 60 samples/s).
const SNAPS: usize = 40;
/// Smooth mode samples 10 times a second and blends two windows (see `dual`).
pub const SHORT_NS: i64 = 200_000_000;
pub const LONG_NS: i64 = 800_000_000;

const DUAL_D0: f32 = 0.05;
const DUAL_D1: f32 = 0.12;

/// Per-series estimator state: held weight and its time, the last deviation.
#[derive(Clone, Copy, Default)]
pub struct EstState {
    hold: f32,
    hold_t: i64,
    prev_d: f32,
}

/// Coasting on a held weight, the fast side is this window (half the short one's
/// tick noise).
pub const MID_NS: i64 = 400_000_000;

/// Smooth-mode load from `ld(a)` = load from time `a` to now. A short window `us`
/// (0.2 s: ±5 % tick quantisation, 0.1 s behind) and a long one `ul` (0.8 s: ±1 %,
/// 0.4 s behind): while they agree within the noise the long one shows, so quiet
/// bars stay still; as they disagree the short one takes over, so real changes show
/// within ~0.3 s. A disagreement within the short window's noise (below DUAL_D1)
/// counts only when the previous sample's had the same sign: a lone tick of noise
/// doesn't move the bar. The weight holds its peak for one long window, until that
/// lies wholly after the change: else the target would sag back towards the lagging
/// long window for a moment after every step; then it fades over another one. While
/// it only coasts on that hold (the short window agrees with the long one again: a
/// ramp or step has ended) the fast side is the 0.4 s window, so the short window's
/// noise can't push the bar past where it settles.
pub fn estimate(st: &mut EstState, now: i64, ld: &dyn Fn(i64) -> f32) -> f32 {
    let (us, ul) = (ld(now - SHORT_NS), ld(now - LONG_NS));
    let d = us - ul;
    let raw = ((d.abs() - DUAL_D0) / (DUAL_D1 - DUAL_D0)).clamp(0.0, 1.0);
    let w = if raw >= 1.0 || (st.prev_d * d > 0.0 && st.prev_d.abs() >= DUAL_D0) {
        raw
    } else {
        0.0
    };
    st.prev_d = d;
    // held, then fading out over a second long window (a sudden drop would stall a
    // bar that is following a ramp)
    let age = (now - st.hold_t - LONG_NS) as f32 / LONG_NS as f32;
    let held = st.hold * (1.0 - age.max(0.0)).max(0.0);
    if w >= held {
        (st.hold, st.hold_t) = (w, now);
        return ul + w * d;
    }
    ul + held * (ld(now - MID_NS) - ul)
}

/// Ring of per-CPU (idle, total) /proc/stat snapshots with their times; load over a
/// sliding window instead of over the last sampling interval.
pub struct StatWindow {
    n: usize,
    t: Vec<i64>,
    v: Vec<(u64, u64)>,
    head: usize,
    len: usize,
}

impl StatWindow {
    pub fn new(n: usize) -> StatWindow {
        StatWindow {
            n,
            t: vec![0; SNAPS],
            v: vec![(0, 0); SNAPS * n],
            head: 0,
            len: 0,
        }
    }
    /// The newest snapshot at least `win` ns older than `now` (tolerating 4 % timer
    /// lateness), else the oldest one. None before the first push.
    pub fn base(&self, now: i64, win: i64) -> Option<&[(u64, u64)]> {
        if self.len == 0 {
            return None;
        }
        let mut pick = (self.head + SNAPS - self.len) % SNAPS;
        for k in 0..self.len {
            let j = (self.head + SNAPS - 1 - k) % SNAPS;
            if now - self.t[j] >= win - win / 25 {
                pick = j;
                break;
            }
        }
        Some(&self.v[pick * self.n..(pick + 1) * self.n])
    }
    pub fn push(&mut self, now: i64, cur: &[(u64, u64)]) {
        let h = self.head;
        self.t[h] = now;
        self.v[h * self.n..(h + 1) * self.n].copy_from_slice(cur);
        self.head = (h + 1) % SNAPS;
        self.len = (self.len + 1).min(SNAPS);
    }
}

/// Smooth-mode clock (see `Sampler::fe`): readings above `floor` are the clock
/// while running (averaged); a floor reading within `hold` ns of the last running
/// one keeps it. `hold` = 0: the raw reading.
#[inline]
pub fn held_clock(shown: f32, f: f32, floor: f32, now: i64, run_t: &mut i64, hold: i64) -> f32 {
    if hold <= 0 {
        return f;
    }
    let held = now - *run_t < hold;
    if f > floor {
        *run_t = now;
        if held {
            shown + (f - shown) * 0.35
        } else {
            f
        }
    } else if held {
        shown
    } else {
        f
    }
}

/// Load of one CPU between two (idle, total) snapshots.
#[inline]
pub fn load(cur: (u64, u64), base: (u64, u64)) -> (f32, u64, u64) {
    let dt = cur.1.saturating_sub(base.1);
    let di = cur.0.saturating_sub(base.0).min(dt);
    let u = if dt > 0 {
        1.0 - di as f32 / dt as f32
    } else {
        0.0
    };
    (u, dt - di, dt)
}

pub fn read_str(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Plain decimal ("4.21", "-0.5", "3") to f32 without core's float parser (its
/// tables cost ~10 KB of binary).
pub fn pf(s: &str) -> Option<f32> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (mut v, mut scale, mut frac, mut any) = (0f64, 1f64, false, false);
    for b in s.bytes() {
        match b {
            b'0'..=b'9' => {
                any = true;
                v = v * 10.0 + (b - b'0') as f64;
                if frac {
                    scale *= 10.0;
                }
            }
            b'.' if !frac => frac = true,
            _ => return None,
        }
    }
    any.then_some(((if neg { -v } else { v }) / scale) as f32)
}

pub fn read_int(path: &str) -> Option<i64> {
    read_str(path)?.parse().ok()
}

pub fn parse_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                out.extend(a..=b);
            }
        } else if let Ok(v) = part.parse() {
            out.push(v);
        }
    }
    out
}

#[cfg(unix)]
pub fn open_ro(path: &str) -> c_int {
    let Ok(c) = CString::new(path) else { return -1 };
    unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) }
}

/// (no procfs or sysfs: the Windows sources use handles, see `win`)
#[cfg(windows)]
pub fn open_ro(_path: &str) -> c_int {
    -1
}

#[cfg(windows)]
pub fn pread(_fd: c_int, _buf: &mut [u8]) -> usize {
    0
}

#[cfg(unix)]
#[inline]
pub fn pread(fd: c_int, buf: &mut [u8]) -> usize {
    if fd < 0 {
        return 0;
    }
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut _, buf.len(), 0) };
    if n < 0 {
        0
    } else {
        n as usize
    }
}

/// Parse a leading unsigned integer (skipping spaces); returns (value, bytes consumed).
#[inline]
pub fn parse_u64(b: &[u8]) -> (u64, usize) {
    let mut i = 0;
    while i < b.len() && b[i] == b' ' {
        i += 1;
    }
    let mut v: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v.wrapping_mul(10).wrapping_add((b[i] - b'0') as u64);
        i += 1;
    }
    (v, i)
}

/// Fixed-capacity ring buffer of history points.
pub struct Ring<T: Copy + Default> {
    buf: Vec<T>,
    head: usize,
    len: usize,
}

impl<T: Copy + Default> Ring<T> {
    pub fn new() -> Self {
        Ring {
            buf: vec![T::default(); HIST],
            head: 0,
            len: 0,
        }
    }
    pub fn push(&mut self, v: T) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % HIST;
        self.len = (self.len + 1).min(HIST);
    }
    pub fn len(&self) -> usize {
        self.len
    }
    /// k = 0 is the newest value.
    #[inline]
    pub fn back(&self, k: usize) -> T {
        self.buf[(self.head + HIST - 1 - k) % HIST]
    }
    /// The last `n` values (oldest first), left-padded with `pad`.
    pub fn tail(&self, n: usize, pad: T, out: &mut Vec<T>) {
        out.clear();
        let have = self.len.min(n);
        out.extend(std::iter::repeat_n(pad, n - have));
        for k in (0..have).rev() {
            out.push(self.back(k));
        }
    }
}

pub struct Sensor {
    pub label: String,
    fd: c_int,
    pub val: f32,
    pub hist: Ring<f32>,
    /// highest point in `hist`
    pub max: f32,
}

#[cfg(test)]
impl Sensor {
    pub(crate) fn fake(label: &str, val: f32) -> Sensor {
        Sensor { label: label.into(), fd: -1, val, hist: Ring::new(), max: 0.0 }
    }
}

pub struct Topo {
    pub cpus: Vec<usize>,
    /// cpu id → index in `cpus` (or usize::MAX)
    pub idx: Vec<usize>,
    /// physical cores: lists of indices into `cpus`
    pub groups: Vec<Vec<usize>>,
    /// L3 domain (≈ CCD) per group
    pub ccd: Vec<usize>,
    pub ndom: usize,
    /// the domains' names where they aren't CCDs (Windows, hybrid CPUs: "P-cores",
    /// "E-cores"); empty: "CCD0", "CCD1" …
    pub dom_names: Vec<&'static str>,
    /// amd-pstate preferred-core ranking per group (-1 = unknown)
    pub rank: Vec<i64>,
    pub maxf: f32,
    pub minf: f32,
    pub boost: f32,
    #[allow(dead_code)]
    pub model: String,
    pub short_model: String,
    pub driver: String,
    pub has_freq: bool,
}

#[cfg(unix)]
fn cpu_model() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    for key in ["model name", "Model", "Hardware", "cpu model", "Processor"] {
        if let Some(l) = info.lines().find(|l| l.starts_with(key) && l.contains(':')) {
            let v = l.split_once(':').unwrap().1.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    if let Some(m) = read_str("/sys/firmware/devicetree/base/model") {
        return m.trim_end_matches('\0').to_string();
    }
    "CPU".to_string()
}

impl Topo {
    /// A domain's name: "CCD0" …, or what the platform calls it ("P-cores").
    pub fn dom_name(&self, d: usize) -> crate::ui::F {
        match self.dom_names.get(d) {
            Some(n) => {
                let mut f = crate::ui::F::new();
                f.str(n);
                f
            }
            None => {
                let mut f = crate::ui::F::new();
                f.str("CCD");
                f.int(d as u32, 0);
                f
            }
        }
    }

    #[cfg(windows)]
    pub fn detect() -> Topo {
        win::topo()
    }

    #[cfg(unix)]
    pub fn detect() -> Topo {
        let mut cpus = read_str(&format!("{SYS}/online"))
            .map(|s| parse_list(&s))
            .unwrap_or_default();
        if cpus.is_empty() {
            // no sysfs: count cpuN lines in /proc/stat
            let st = std::fs::read_to_string("/proc/stat").unwrap_or_default();
            cpus = st
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("cpu")
                        .and_then(|r| r.split(' ').next()?.parse().ok())
                })
                .collect();
        }
        if cpus.is_empty() {
            cpus.push(0);
        }
        let maxid = *cpus.iter().max().unwrap();
        let mut idx = vec![usize::MAX; maxid + 1];
        for (i, &c) in cpus.iter().enumerate() {
            idx[c] = i;
        }
        let fi = |c: usize, n: &str| read_int(&format!("{SYS}/cpu{c}/cpufreq/{n}"));
        let maxf = cpus
            .iter()
            .filter_map(|&c| fi(c, "cpuinfo_max_freq"))
            .max()
            .unwrap_or(0) as f32
            / 1000.0;
        let minf = cpus
            .iter()
            .filter_map(|&c| fi(c, "cpuinfo_min_freq"))
            .min()
            .unwrap_or(0) as f32
            / 1000.0;
        let has_freq = maxf > 0.0;
        let maxf = if has_freq { maxf } else { 1.0 };
        // physical cores from thread_siblings_list
        let mut seen = vec![false; cpus.len()];
        let mut groups = Vec::new();
        for (i, &c) in cpus.iter().enumerate() {
            if seen[i] {
                continue;
            }
            let sib = read_str(&format!("{SYS}/cpu{c}/topology/thread_siblings_list"))
                .map(|s| parse_list(&s))
                .unwrap_or_default();
            let mut g: Vec<usize> = sib
                .iter()
                .filter(|&&s| s <= maxid && idx[s] != usize::MAX)
                .map(|&s| idx[s])
                .collect();
            if !g.contains(&i) {
                g = vec![i];
            }
            g.retain(|&j| !seen[j]);
            for &j in &g {
                seen[j] = true;
            }
            groups.push(g);
        }
        // last-level cache (L3 if present) shared_cpu_list per group → domain index
        let llc = |c: usize| -> String {
            for k in (0..8).rev() {
                let base = format!("{SYS}/cpu{c}/cache/index{k}");
                if read_int(&format!("{base}/level")) == Some(3) {
                    return read_str(&format!("{base}/shared_cpu_list")).unwrap_or_default();
                }
            }
            String::new()
        };
        let keys: Vec<String> = groups.iter().map(|g| llc(cpus[g[0]])).collect();
        let mut order: Vec<&String> = Vec::new();
        for k in &keys {
            if !order.contains(&k) {
                order.push(k);
            }
        }
        let ccd: Vec<usize> = keys
            .iter()
            .map(|k| order.iter().position(|o| *o == k).unwrap())
            .collect();
        let ndom = order.len();
        let rank = groups
            .iter()
            .map(|g| fi(cpus[g[0]], "amd_pstate_prefcore_ranking").unwrap_or(-1))
            .collect();
        let model = cpu_model();
        let short_model = model
            .replace("(R)", "")
            .replace("(TM)", "")
            .split_whitespace()
            .filter(|w| *w != "Processor" && *w != "CPU" && !w.ends_with("-Core"))
            .collect::<Vec<_>>()
            .join(" ");
        Topo {
            idx,
            groups,
            ccd,
            ndom,
            dom_names: Vec::new(),
            rank,
            maxf,
            minf,
            boost: 0.9 * maxf,
            model,
            short_model,
            driver: read_str(&format!("{SYS}/cpu0/cpufreq/scaling_driver")).unwrap_or_default(),
            has_freq,
            cpus,
        }
    }
}

pub struct Sampler {
    pub t: Topo,
    stat_fd: c_int,
    stat_buf: Vec<u8>,
    freq_fds: Vec<c_int>,
    win: StatWindow,
    step_prev: Vec<(u64, u64)>,
    cur: Vec<(u64, u64)>,
    /// latest sample: clock (MHz) and windowed load (0..1) per CPU
    pub fr: Vec<f32>,
    pub ur: Vec<f32>,
    /// shown clock. scaling_cur_freq of a lightly loaded thread flips between the
    /// idle floor (it happened to be idle at the last tick) and its running clock
    /// from one read to the next. With `clock_hold` > 0 (smooth mode) `fe` is the
    /// clock while running: an average of the readings above the floor, held for
    /// `clock_hold` ns after the last one. 0 = the raw reading.
    pub fe: Vec<f32>,
    pub clock_hold: i64,
    /// smooth mode: `ur` / `tot_r` from two windows (`dual`) instead of WINDOW_NS
    pub dual: bool,
    /// `estimate` state per CPU, and for the total (last)
    est: Vec<EstState>,
    /// monotonic time of the last history point
    pub push_t: i64,
    run_t: Vec<i64>,
    /// published values: what numbers, sorting and summaries show. `publish()` copies
    /// the latest sample here (every sample, or a few times a second in smooth mode
    /// so the digits stay readable).
    pub f: Vec<f32>,
    pub u: Vec<f32>,
    pub peak: Vec<f32>,
    pub hf: Vec<Ring<u16>>,
    pub hu: Vec<Ring<u8>>,
    acc_f: Vec<f32>,
    acc_n: u32,
    tot_r: f32,
    pub avg_r: f32,
    pub tot_u: f32,
    pub avg_f: f32,
    /// largest per-CPU load change in the last sample (0..1): drives the sample rate
    pub activity: f32,
    /// number of history points pushed so far
    pub hist_n: u64,
    pub h_tot: Ring<f32>,
    pub h_avg: Ring<f32>,
    pub temps: Vec<Sensor>,
    /// each core's temperature (°C, 0: none), where they are known (Windows, with
    /// cpuview's sensor service)
    pub core_t: Vec<f32>,
    pub gov: String,
    pub epp: String,
    pub loadavg: [f32; 3],
    loadavg_fd: c_int,
    gov_fd: c_int,
    epp_fd: c_int,
    temp_rr: usize,
    freq_rr: usize,
    /// SMT sibling of each CPU index (first other thread of the same core)
    sibling: Vec<usize>,
    f_copy: Option<(usize, usize)>,
    /// Windows: the system calls and counters that stand in for /proc and sysfs,
    /// and whether the speed is eco (the clocks' counter collection is dear)
    #[cfg(windows)]
    w: win::Cpu,
    #[cfg(windows)]
    pub eco: bool,
}

/// (Windows has no CPU temperature interface without a driver: cpuview's sensor
/// service's come later, see win::Cpu::svc_temps)
#[cfg(windows)]
fn find_sensors() -> Vec<Sensor> {
    win::find_sensors()
}

#[cfg(unix)]
fn find_sensors() -> Vec<Sensor> {
    let mut out = Vec::new();
    let wanted = [
        "k10temp",
        "zenpower",
        "coretemp",
        "cpu_thermal",
        "cpu-thermal",
        "soc_thermal",
    ];
    for d in hwmon_dirs() {
        let name = read_str(&format!("{d}/name")).unwrap_or_default();
        if !wanted.contains(&name.as_str()) {
            continue;
        }
        for num in 1..32 {
            let inp = format!("temp{num}_input");
            if !std::path::Path::new(&format!("{d}/{inp}")).exists() {
                continue;
            }
            let label = read_str(&format!("{d}/temp{num}_label"));
            let label = match label {
                Some(l) if name == "coretemp" && l.starts_with("Core") => continue, // per-core: too many
                Some(l) => l.replace("Package id ", "Pkg"),
                None => {
                    if name == "k10temp" || name == "zenpower" {
                        continue;
                    }
                    "CPU".to_string()
                }
            };
            let fd = open_ro(&format!("{d}/{inp}"));
            if fd >= 0 {
                out.push(Sensor {
                    label,
                    fd,
                    val: 0.0,
                    hist: Ring::new(),
                    max: 0.0,
                });
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

/// hwmon device directories in number order (no sorting: std's sort costs ~4 KB of
/// binary per element type).
pub fn hwmon_dirs() -> Vec<String> {
    let mut v = Vec::new();
    let mut miss = 0;
    for i in 0..256 {
        let d = format!("/sys/class/hwmon/hwmon{i}");
        if std::path::Path::new(&d).exists() {
            v.push(d);
            miss = 0;
        } else {
            miss += 1;
            if miss > 8 {
                break;
            }
        }
    }
    v
}

/// Stable insertion sort for short lists (one small copy per element type instead
/// of std's ~4 KB sort machinery).
pub fn isort<T>(v: &mut [T], lt: &dyn Fn(&T, &T) -> bool) {
    for i in 1..v.len() {
        let mut j = i;
        while j > 0 && lt(&v[j], &v[j - 1]) {
            v.swap(j, j - 1);
            j -= 1;
        }
    }
}

/// Parse the per-CPU lines of /proc/stat into (idle + iowait, total) jiffies.
pub fn parse_stat(b: &[u8], idx: &[usize], cur: &mut [(u64, u64)]) {
    let mut i = 0;
    // skip the aggregate "cpu " line
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    i += 1;
    while i + 3 < b.len() && &b[i..i + 3] == b"cpu" {
        i += 3;
        let (id, k) = parse_u64(&b[i..]);
        i += k;
        let mut v = [0u64; 8];
        for x in v.iter_mut() {
            let (val, k) = parse_u64(&b[i..]);
            *x = val;
            i += k;
        }
        while i < b.len() && b[i] != b'\n' {
            i += 1;
        }
        i += 1;
        let id = id as usize;
        if id < idx.len() && idx[id] != usize::MAX {
            cur[idx[id]] = (v[3] + v[4], v.iter().sum());
        }
    }
}

impl Sampler {
    pub fn new() -> Sampler {
        let t = Topo::detect();
        let n = t.cpus.len();
        let freq_fds = t
            .cpus
            .iter()
            .map(|&c| open_ro(&format!("{SYS}/cpu{c}/cpufreq/scaling_cur_freq")))
            .collect();
        let stat_fd = open_ro("/proc/stat");
        let mut s = Sampler {
            stat_fd,
            stat_buf: vec![0; 4096],
            freq_fds,
            win: StatWindow::new(n),
            step_prev: vec![(0, 0); n],
            cur: vec![(0, 0); n],
            fr: vec![0.0; n],
            ur: vec![0.0; n],
            fe: vec![0.0; n],
            clock_hold: 0,
            dual: false,
            est: vec![EstState::default(); n + 1],
            push_t: 0,
            run_t: vec![i64::MIN / 2; n],
            f: vec![0.0; n],
            u: vec![0.0; n],
            peak: vec![0.0; n],
            hf: (0..n).map(|_| Ring::new()).collect(),
            hu: (0..n).map(|_| Ring::new()).collect(),
            acc_f: vec![0.0; n],
            acc_n: 0,
            tot_r: 0.0,
            avg_r: 0.0,
            tot_u: 0.0,
            avg_f: 0.0,
            activity: 1.0,
            hist_n: 0,
            h_tot: Ring::new(),
            h_avg: Ring::new(),
            temps: find_sensors(),
            core_t: Vec::new(),
            gov: String::new(),
            epp: String::new(),
            loadavg: [0.0; 3],
            loadavg_fd: open_ro("/proc/loadavg"),
            gov_fd: -1,
            epp_fd: -1,
            temp_rr: 0,
            freq_rr: 0,
            sibling: Vec::new(),
            f_copy: None,
            #[cfg(windows)]
            w: win::Cpu::new(&t),
            #[cfg(windows)]
            eco: false,
            t,
        };
        // size the /proc/stat buffer once (it can be large on many-CPU machines)
        loop {
            let len = s.stat_buf.len();
            let got = pread(s.stat_fd, &mut s.stat_buf);
            if got < len {
                let want = (got + got / 2 + 1024).max(4096);
                s.stat_buf.resize(want, 0);
                break;
            }
            s.stat_buf.resize(len * 2, 0);
        }
        s.read_stat();
        s.win.push(crate::term::mono(), &s.cur);
        s.step_prev.copy_from_slice(&s.cur);
        s.sibling = vec![usize::MAX; n];
        for g in &s.t.groups {
            if g.len() == 2 {
                s.sibling[g[0]] = g[1];
                s.sibling[g[1]] = g[0];
            }
        }
        let c0 = s.t.cpus[0];
        s.gov_fd = open_ro(&format!("{SYS}/cpu{c0}/cpufreq/scaling_governor"));
        s.epp_fd = open_ro(&format!(
            "{SYS}/cpu{c0}/cpufreq/energy_performance_preference"
        ));
        s.read_slow();
        s.read_policy();
        s.read_temps(true);
        s
    }

    #[cfg(windows)]
    fn read_stat(&mut self) {
        self.w.times(&mut self.cur);
    }

    #[cfg(unix)]
    fn read_stat(&mut self) {
        let mut n = pread(self.stat_fd, &mut self.stat_buf);
        if n == self.stat_buf.len() {
            // grew (CPU hotplug, many IRQs): enlarge once, outside the steady state
            let l = self.stat_buf.len();
            self.stat_buf.resize(l * 2, 0);
            n = pread(self.stat_fd, &mut self.stat_buf);
        }
        parse_stat(&self.stat_buf[..n], &self.t.idx, &mut self.cur);
    }

    /// Clocks. Each read costs ~1.5 µs on a cold core, so only CPUs that were busy
    /// for at least 3% of the load window are read every sample; barely-busy ones are
    /// read round-robin (a quarter per sample) and fully idle ones show their
    /// minimum clock, which is what scaling_cur_freq reports for them anyway.
    /// (Windows: one counter collection for all of them, see win::Cpu::clocks)
    /// (eco: every other sample; a collection costs 30-100 µs, most of a sample)
    #[cfg(windows)]
    fn read_freq(&mut self) {
        self.freq_rr = self.freq_rr.wrapping_add(1);
        if self.eco && self.freq_rr % 2 == 0 {
            return;
        }
        self.w.clocks(&self.ur, &mut self.fr, &mut self.t);
    }

    #[cfg(unix)]
    fn read_freq(&mut self) {
        let mut buf = [0u8; 24];
        let n = self.freq_fds.len();
        self.freq_rr = self.freq_rr.wrapping_add(1);
        for i in 0..n {
            let u = self.ur[i];
            if self.fr[i] > 0.0 && self.t.has_freq {
                if u <= 0.0 {
                    self.fr[i] = self.t.minf;
                    continue;
                }
                if u < 0.03 && (i + self.freq_rr) % 4 != 0 {
                    continue;
                }
                // SMT siblings that were both busy most of the window run on the same
                // core clock: read one of them (alternating) and copy it to the other.
                let sib = self.sibling[i];
                if sib != usize::MAX && u >= 0.5 && self.ur[sib] >= 0.5 {
                    let first = i.min(sib);
                    let pick = if self.freq_rr % 2 == 0 {
                        first
                    } else {
                        i.max(sib)
                    };
                    if i != pick {
                        if i > pick {
                            self.fr[i] = self.fr[pick];
                        } else {
                            self.f_copy = Some((pick, i));
                        }
                        continue;
                    }
                }
            }
            let r = pread(self.freq_fds[i], &mut buf);
            self.fr[i] = parse_u64(&buf[..r]).0 as f32 / 1000.0;
            if let Some((p, d)) = self.f_copy {
                if p == i {
                    self.fr[d] = self.fr[i];
                    self.f_copy = None;
                }
            }
        }
    }

    /// One live sample into `fr` / `ur` / `fe` (see `publish`). `push` = number of
    /// history points (seconds) this sample closes; `clocks`: read the clocks too.
    pub fn sample(&mut self, push: u32, clocks: bool) {
        self.read_stat();
        let now = crate::term::mono();
        self.window_load(now);
        if clocks {
            self.read_freq();
        }
        let mut fsum = 0.0;
        let floor = self.t.minf * 1.1 + 1.0;
        for i in 0..self.cur.len() {
            let f = self.fr[i];
            self.fe[i] = held_clock(
                self.fe[i],
                f,
                floor,
                now,
                &mut self.run_t[i],
                self.clock_hold,
            );
            fsum += f;
            self.acc_f[i] += f;
            if f > self.peak[i] {
                self.peak[i] = f;
            }
        }
        self.acc_n += 1;
        self.avg_r = fsum / self.fr.len().max(1) as f32;
        if push > 0 {
            self.push_t = now;
            self.push_history(push);
        }
    }

    /// Load over the sliding window, then remember this snapshot.
    fn window_load(&mut self, now: i64) {
        let frac = |b: u64, t: u64| if t > 0 { b as f32 / t as f32 } else { 0.0 };
        let (mut busy, mut total) = (0u64, 0u64);
        let mut act = 0.0f32;
        let w = &self.win;
        if let Some(base) = w.base(now, if self.dual { SHORT_NS } else { WINDOW_NS }) {
            for (i, (&c, &b)) in self.cur.iter().zip(base).enumerate() {
                let (mut u, bz, dt) = load(c, b);
                busy += bz;
                total += dt;
                if self.dual {
                    let ld = |a: i64| w.base(now, now - a).map_or(u, |x| load(c, x[i]).0);
                    u = estimate(&mut self.est[i], now, &ld);
                }
                act = act.max((u - self.ur[i]).abs());
                self.ur[i] = u;
            }
        }
        self.tot_r = frac(busy, total);
        if self.dual {
            let cur = &self.cur;
            let ld = |a: i64| {
                w.base(now, now - a).map_or(0.0, |x| {
                    let (mut bb, mut tt) = (0u64, 0u64);
                    for (&p, &q) in cur.iter().zip(x) {
                        let (_, bz, dt) = load(p, q);
                        (bb, tt) = (bb + bz, tt + dt);
                    }
                    frac(bb, tt)
                })
            };
            let k = self.est.len() - 1;
            self.tot_r = estimate(&mut self.est[k], now, &ld);
        }
        self.win.push(now, &self.cur);
        self.activity = act;
    }

    /// Load of CPU `i` over the current (unfinished) history second.
    pub fn part_u(&self, i: usize) -> f32 {
        load(self.cur[i], self.step_prev[i]).0
    }

    /// Total load over the current history second.
    pub fn part_tot(&self) -> f32 {
        let (mut b, mut t) = (0u64, 0u64);
        for (&c, &p) in self.cur.iter().zip(&self.step_prev) {
            let (_, bz, dt) = load(c, p);
            (b, t) = (b + bz, t + dt);
        }
        if t > 0 {
            b as f32 / t as f32
        } else {
            0.0
        }
    }

    /// Mean clock reading of CPU `i` over the current history second.
    pub fn part_f(&self, i: usize) -> f32 {
        if self.acc_n > 0 {
            self.acc_f[i] / self.acc_n as f32
        } else {
            self.fr[i]
        }
    }

    /// Make the latest sample the shown one (numbers, sorting, summaries).
    pub fn publish(&mut self) {
        self.f.copy_from_slice(&self.fe);
        self.u.copy_from_slice(&self.ur);
        self.tot_u = self.tot_r;
        self.avg_f = self.avg_r;
    }

    fn push_history(&mut self, times: u32) {
        let n = self.acc_n.max(1) as f32;
        let (mut busy, mut total) = (0u64, 0u64);
        let mut fsum = 0.0;
        for i in 0..self.cur.len() {
            let (u, bz, dt) = load(self.cur[i], self.step_prev[i]);
            busy += bz;
            total += dt;
            let f = self.acc_f[i] / n;
            fsum += f;
            for _ in 0..times {
                self.hf[i].push(f.round().clamp(0.0, 65535.0) as u16);
                self.hu[i].push((u * 255.0 + 0.5) as u8);
            }
            self.acc_f[i] = 0.0;
        }
        self.acc_n = 0;
        self.hist_n += 1;
        self.step_prev.copy_from_slice(&self.cur);
        for _ in 0..times {
            self.h_tot.push(if total > 0 {
                busy as f32 / total as f32
            } else {
                0.0
            });
            self.h_avg.push(fsum / self.cur.len().max(1) as f32);
            for s in &mut self.temps {
                s.hist.push(s.val);
            }
        }
        for s in &mut self.temps {
            s.max = (0..s.hist.len())
                .map(|k| s.hist.back(k))
                .fold(s.val, f32::max);
        }
    }

    /// Temperatures (Windows: cpuview's sensor service's, see win::Cpu::svc_temps).
    #[cfg(windows)]
    pub fn read_temps(&mut self, _all: bool) {
        self.w.svc_temps(&mut self.temps, &mut self.core_t, &self.t);
    }

    /// Temperatures: k10temp reads go over SMN and cost ~10 µs each on a cold core,
    /// so the caller reads one sensor at a time (round robin). `all` reads every one.
    #[cfg(unix)]
    pub fn read_temps(&mut self, all: bool) {
        let mut buf = [0u8; 16];
        let n = self.temps.len();
        for k in 0..if all { n } else { n.min(1) } {
            let i = (self.temp_rr + k) % n;
            let s = &mut self.temps[i];
            let r = pread(s.fd, &mut buf);
            let (v, c) = parse_u64(&buf[..r]);
            if c > 0 {
                s.val = v as f32 / 1000.0;
            }
        }
        if n > 0 {
            self.temp_rr = (self.temp_rr + 1) % n;
        }
    }

    /// Load average (Windows: estimated, see win::Cpu::loadavg).
    #[cfg(windows)]
    pub fn read_slow(&mut self) {
        let busy = self.tot_r * self.cur.len() as f32;
        self.w.loadavg(busy, crate::term::mono(), &mut self.loadavg);
    }

    /// Load average (the kernel updates it every 5 s).
    #[cfg(unix)]
    pub fn read_slow(&mut self) {
        let mut buf = [0u8; 64];
        let n = pread(self.loadavg_fd, &mut buf);
        let s = std::str::from_utf8(&buf[..n]).unwrap_or("");
        for (i, p) in s.split(' ').take(3).enumerate() {
            self.loadavg[i] = pf(p).unwrap_or(0.0);
        }
    }

    /// The power plan (Windows has no governor or EPP).
    #[cfg(windows)]
    pub fn read_policy(&mut self) {
        self.w.policy(&mut self.gov);
    }

    /// Governor and EPP (rarely change; persistent fds, compared before allocating).
    #[cfg(unix)]
    pub fn read_policy(&mut self) {
        let mut buf = [0u8; 64];
        for (fd, dst, dflt) in [
            (self.gov_fd, &mut self.gov, "?"),
            (self.epp_fd, &mut self.epp, ""),
        ] {
            let n = pread(fd, &mut buf);
            let v = std::str::from_utf8(&buf[..n]).unwrap_or("").trim();
            let v = if n == 0 { dflt } else { v };
            if dst != v {
                *dst = v.to_string();
            }
        }
    }

    pub fn reset_peaks(&mut self) {
        self.peak.copy_from_slice(&self.fr);
    }

    /// Temperature for an L3 domain: Tccd<n> on Ryzen, else Tctl / package.
    /// Index of the sensor for an L3 domain (no allocation: called on every frame).
    pub fn domain_temp_idx(&self, d: usize) -> Option<usize> {
        let ccd = |l: &str| l.strip_prefix("Tccd").and_then(|n| n.parse::<usize>().ok());
        let t = &self.temps;
        let named = self.t.dom_names.get(d).copied().unwrap_or("");
        t.iter()
            .position(|s| ccd(&s.label) == Some(d + 1) || (!named.is_empty() && named.starts_with(s.label.as_str())))
            .or_else(|| {
                t.iter()
                    .position(|s| s.label == "Tctl" || s.label.starts_with("Pkg"))
            })
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    #[test]
    fn lists_and_ints() {
        assert_eq!(parse_list("0-3,8,10-11\n"), vec![0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(parse_list(""), Vec::<usize>::new());
        assert_eq!(parse_u64(b"  12345 6"), (12345, 7));
        assert_eq!(parse_u64(b"5530000\n").0, 5530000);
    }
    #[test]
    fn ring() {
        let mut r: Ring<u8> = Ring::new();
        for i in 0..2000u32 {
            r.push((i % 256) as u8);
        }
        assert_eq!(r.len(), HIST);
        assert_eq!(r.back(0), (1999 % 256) as u8);
        let mut v = Vec::new();
        r.tail(3, 0, &mut v);
        assert_eq!(
            v,
            vec![(1997 % 256) as u8, (1998 % 256) as u8, (1999 % 256) as u8]
        );
        let mut r2: Ring<u8> = Ring::new();
        r2.push(7);
        r2.tail(3, 0, &mut v);
        assert_eq!(v, vec![0, 0, 7]);
    }
    /// A CPU 30 % busy (3 ms of every 10 ms) as /proc/stat shows it: whole 10 ms
    /// ticks, truncated. Sampled at 60/s the per-interval load is 0/50/100 %; over
    /// the sliding window it stays within a few points of 30 %.
    #[test]
    fn windowed_load_is_accurate_at_60_per_second() {
        let tick = 10_000_000i64; // USER_HZ = 100
        let busy_ns = |t: i64| (t / 10_000_000) * 3_000_000 + (t % 10_000_000).min(3_000_000);
        let snap = |t: i64| {
            let b = busy_ns(t) as u64 / tick as u64;
            let total = t as u64 / tick as u64;
            (total - b, total) // (idle, total) in ticks
        };
        let mut w = StatWindow::new(1);
        let period = 1_000_000_000 / 60;
        let (mut lo, mut hi) = (1.0f32, 0.0f32);
        let (mut raw_lo, mut raw_hi) = (1.0f32, 0.0f32);
        let mut prev = snap(0);
        for k in 0..600i64 {
            // a little timer jitter
            let t = 1_000_000_000 + k * period + (k * 7919 % 2_000_000);
            let cur = snap(t);
            if let Some(base) = w.base(t, WINDOW_NS) {
                if k > 40 {
                    let u = load(cur, base[0]).0;
                    (lo, hi) = (lo.min(u), hi.max(u));
                    let r = load(cur, prev).0;
                    (raw_lo, raw_hi) = (raw_lo.min(r), raw_hi.max(r));
                }
            }
            w.push(t, &[cur]);
            prev = cur;
        }
        assert!(
            raw_lo == 0.0 && raw_hi == 1.0,
            "per-interval load quantises"
        );
        assert!(lo >= 0.27 && hi <= 0.33, "windowed load {lo}..{hi}");
    }

    /// A CPU ramping 10 → 90 % over 3 s, then steady, as /proc/stat shows it (whole
    /// 10 ms ticks of a 10 ms PWM, sampled 10 times a second with timer jitter): the
    /// estimate followed by the bar spring must not overshoot where it settles (the
    /// short window's tick noise used to push it ~3 % past at the end of the ramp).
    #[test]
    fn ramp_end_does_not_overshoot() {
        use crate::anim::{spring, FPS, W_LOAD};
        let tick = 10_000_000i64;
        let duty = |t: i64| {
            let s = t as f64 / 1e9;
            if s < 3.0 {
                0.1
            } else if s < 6.0 {
                0.1 + 0.8 * (s - 3.0) / 3.0
            } else {
                0.9
            }
        };
        let mut worst = 0.0f32;
        for seed in 0..12u64 {
            // busy time of a 10 ms PWM (busy first), counted in whole 10 ms ticks
            let phase = (seed * 3_700_000) as i64 % tick;
            let busy_ticks = |t: i64| -> u64 {
                let mut ns = 0i64;
                let mut p = -phase;
                while p < t {
                    // ± 15 % per period: preemption, wakeup latency
                    let r = ((p / tick) as u64 ^ seed.wrapping_mul(0x9E37_79B9))
                        .wrapping_mul(0x2545_F491_4F6C_DD1D)
                        >> 40;
                    let jit = (r % 1000) as f64 / 1000.0 * 0.3 - 0.15;
                    let on = ((duty(p.max(0)) + jit).clamp(0.0, 1.0) * tick as f64) as i64;
                    ns += on.min(t - p).max(0);
                    p += tick;
                }
                (ns / tick) as u64
            };
            let mut w = StatWindow::new(1);
            let mut st = EstState::default();
            let (mut x, mut v, mut tgt) = (0.1f32, 0.0f32, 0.1f32);
            let dt = 1.0 / FPS as f32;
            let ex = (-W_LOAD * dt).exp();
            let mut lcg = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let (mut maxx, mut settled) = (0.0f32, 0.0f32);
            for f in 0..(9 * FPS) {
                let now = f * 1_000_000_000 / FPS;
                if f % 6 == 0 {
                    lcg = lcg
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let t = now + (lcg >> 40) as i64 % 3_000_000;
                    let total = (t / tick) as u64;
                    let cur = (total - busy_ticks(t), total);
                    if t > 1_000_000_000 {
                        let ld = |a: i64| w.base(t, t - a).map_or(0.0, |b| load(cur, b[0]).0);
                        let n = estimate(&mut st, t, &ld);
                        if (n - tgt).abs() >= 0.008 {
                            tgt = n;
                        }
                    }
                    w.push(t, &[cur]);
                }
                spring(&mut x, &mut v, tgt, dt, W_LOAD, ex, 1e-5);
                let s = now as f32 / 1e9;
                if (6.0..8.0).contains(&s) {
                    maxx = maxx.max(x);
                }
                if (8.0..9.0).contains(&s) {
                    settled = settled.max(x);
                }
            }
            worst = worst.max(maxx - settled);
        }
        // (the old estimator: 2.4 %; this one stays within the load's own noise)
        assert!(worst < 0.016, "ramp end overshoots by {worst}");
    }

    #[test]
    fn smooth_clock_holds_the_running_clock() {
        let s = 1_000_000_000i64;
        let mut rt = i64::MIN / 2;
        let floor = 700.0;
        assert_eq!(held_clock(0.0, 620.0, floor, 0, &mut rt, 0), 620.0, "raw");
        let mut f = held_clock(0.0, 5500.0, floor, 0, &mut rt, s);
        assert_eq!(f, 5500.0);
        // idle-floor readings in between keep the running clock
        f = held_clock(f, 620.0, floor, s / 5, &mut rt, s);
        assert_eq!(f, 5500.0);
        // running readings are averaged
        f = held_clock(f, 5300.0, floor, 2 * s / 5, &mut rt, s);
        assert!(f > 5300.0 && f < 5500.0);
        // only the floor for longer than `hold`: idle
        assert_eq!(held_clock(f, 620.0, floor, 2 * s, &mut rt, s), 620.0);
    }

    #[test]
    fn window_picks_the_newest_old_enough_snapshot() {
        let mut w = StatWindow::new(2);
        assert!(w.base(0, WINDOW_NS).is_none());
        let ms = 1_000_000i64;
        for k in 0..10i64 {
            w.push(k * 100 * ms, &[(k as u64, 0), (0, k as u64)]);
        }
        // at 1000 ms: snapshots 0..900 ms exist; the newest ≥ 480 ms old is 500 ms
        assert_eq!(w.base(1000 * ms, WINDOW_NS).unwrap()[0].0, 5);
        // a slow speed: the previous sample is the base
        assert_eq!(w.base(1900 * ms, WINDOW_NS).unwrap()[0].0, 9);
        // the ring wraps: only the last SNAPS remain, the oldest is used if needed
        for k in 10..100i64 {
            w.push(k * 10 * ms, &[(k as u64, 0), (0, 0)]);
        }
        assert_eq!(
            w.base(990 * ms + 1, 100 * WINDOW_NS).unwrap()[0].0,
            100 - SNAPS as u64
        );
    }

    #[test]
    fn proc_stat_parse() {
        let text = b"cpu  10 0 10 100 0 0 0 0 0 0\ncpu0 1 2 3 40 5 6 7 8 9 10\ncpu2 10 0 0 90 0 0 0 0 0 0\nintr 123 4 5\n";
        let mut s = Sampler::new_for_test(&[0, 2]);
        s.stat_buf = text.to_vec();
        s.parse_stat_buf(text.len());
        assert_eq!(s.cur[0], (45, 1 + 2 + 3 + 40 + 5 + 6 + 7 + 8));
        assert_eq!(s.cur[1], (90, 100));
    }
}

#[cfg(test)]
impl Sampler {
    pub(crate) fn new_for_test(cpus: &[usize]) -> Sampler {
        let maxid = *cpus.iter().max().unwrap();
        let mut idx = vec![usize::MAX; maxid + 1];
        for (i, &c) in cpus.iter().enumerate() {
            idx[c] = i;
        }
        let n = cpus.len();
        let t = Topo {
            cpus: cpus.to_vec(),
            idx,
            groups: (0..n).map(|i| vec![i]).collect(),
            ccd: vec![0; n],
            ndom: 1,
            dom_names: Vec::new(),
            rank: vec![-1; n],
            maxf: 1.0,
            minf: 0.0,
            boost: 0.9,
            model: "Test".into(),
            short_model: "Test".into(),
            driver: String::new(),
            has_freq: false,
        };
        Sampler {
            t,
            stat_fd: -1,
            stat_buf: Vec::new(),
            freq_fds: vec![-1; n],
            win: StatWindow::new(n),
            step_prev: vec![(0, 0); n],
            cur: vec![(0, 0); n],
            fr: vec![0.0; n],
            ur: vec![0.0; n],
            fe: vec![0.0; n],
            clock_hold: 0,
            dual: false,
            est: vec![EstState::default(); n + 1],
            push_t: 0,
            run_t: vec![i64::MIN / 2; n],
            f: vec![0.0; n],
            u: vec![0.0; n],
            peak: vec![0.0; n],
            hf: (0..n).map(|_| Ring::new()).collect(),
            hu: (0..n).map(|_| Ring::new()).collect(),
            acc_f: vec![0.0; n],
            acc_n: 0,
            tot_r: 0.0,
            avg_r: 0.0,
            tot_u: 0.0,
            avg_f: 0.0,
            activity: 1.0,
            hist_n: 0,
            h_tot: Ring::new(),
            h_avg: Ring::new(),
            temps: Vec::new(),
            core_t: Vec::new(),
            gov: String::new(),
            epp: String::new(),
            loadavg: [0.0; 3],
            loadavg_fd: -1,
            gov_fd: -1,
            epp_fd: -1,
            temp_rr: 0,
            freq_rr: 0,
            sibling: vec![usize::MAX; n],
            f_copy: None,
            #[cfg(windows)]
            w: win::Cpu::none(),
            #[cfg(windows)]
            eco: false,
        }
    }
    fn parse_stat_buf(&mut self, n: usize) {
        let _ = n;
        // reuse the parser on an in-memory buffer
        parse_stat(&self.stat_buf[..n], &self.t.idx, &mut self.cur);
    }
}

#[cfg(test)]
impl Sampler {
    /// Push a history point from the current f/u values (tests).
    pub(crate) fn sample_fake(&mut self) {
        for i in 0..self.f.len() {
            self.hf[i].push(self.f[i] as u16);
            self.hu[i].push((self.u[i] * 255.0) as u8);
        }
        self.h_tot.push(0.3);
        self.h_avg.push(3000.0);
        self.hist_n += 1;
    }
}

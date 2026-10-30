//! Memory, swap, disks, filesystems and the biggest processes: /proc and sysfs read
//! through persistent file descriptors into preallocated buffers, like `sys`.
//!
//! Costs (Ryzen 9950X, 32 threads): /proc/meminfo 1.5 µs, a disk's sysfs `stat` 0.9 µs
//! (the same counters as /proc/diskstats, which costs 8 µs as it walks every block
//! device), /proc/vmstat 10 µs, PSI 1 µs, statvfs 0.5 µs a filesystem; the process
//! scan ~1.3 µs per process. DIMM (spd5118 / jc42, SMBus) and drive (nvme / drivetemp)
//! temperatures cost 40–65 µs of CPU and 1–20 ms of wall time per read, so a helper
//! thread reads them every few seconds and the main loop never waits for them.

// (the Linux parsers stay compiled, and tested, on Windows too)
#![cfg_attr(windows, allow(dead_code, unused_imports))]
use crate::sys::{open_ro, parse_u64, pread, Ring};
use std::ffi::CString;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering::Relaxed};

/// The Windows sources (system calls and device queries instead of /proc and sysfs).
#[cfg(windows)]
#[path = "win/mem.rs"]
mod win;

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct MemInfo {
    pub total: u64,
    pub free: u64,
    pub avail: u64,
    pub buffers: u64,
    pub cached: u64,
    pub sreclaim: u64,
    pub shmem: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl MemInfo {
    /// As `free` (procps-ng 4) reports them.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.avail)
    }
    pub fn cache(&self) -> u64 {
        self.buffers + self.cached + self.sreclaim
    }
    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

/// Leading key of a `key value` / `key: value` line, its value, and the next line.
fn kv(b: &[u8], mut i: usize) -> (&[u8], u64, usize) {
    let s = i;
    while i < b.len() && b[i] != b':' && b[i] != b' ' && b[i] != b'\n' {
        i += 1;
    }
    let k = &b[s..i];
    if i < b.len() && b[i] == b':' {
        i += 1;
    }
    let (v, n) = parse_u64(&b[i..]);
    i += n;
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    (k, v, i + 1)
}

pub fn parse_meminfo(b: &[u8], m: &mut MemInfo) {
    let mut i = 0;
    let mut avail = None;
    while i < b.len() {
        let (k, v, n) = kv(b, i);
        i = n;
        let v = v * 1024;
        match k {
            b"MemTotal" => m.total = v,
            b"MemFree" => m.free = v,
            b"MemAvailable" => avail = Some(v),
            b"Buffers" => m.buffers = v,
            b"Cached" => m.cached = v,
            b"SReclaimable" => m.sreclaim = v,
            b"Shmem" => m.shmem = v,
            b"SwapTotal" => m.swap_total = v,
            b"SwapFree" => m.swap_free = v,
            _ => {}
        }
    }
    // (kernels before 3.14 have no MemAvailable)
    m.avail = avail.unwrap_or(m.free + m.buffers + m.cached);
}

/// pswpin / pswpout (pages) from /proc/vmstat.
pub fn parse_vmstat(b: &[u8]) -> (u64, u64) {
    let (mut i, mut r) = (0, (0, 0));
    while i < b.len() {
        let (k, v, n) = kv(b, i);
        i = n;
        match k {
            b"pswpin" => r.0 = v,
            b"pswpout" => {
                r.1 = v;
                break;
            }
            _ => {}
        }
    }
    r
}

/// "some avg10=1.23 ..." → 1.23 (% of time some task stalled on memory, last 10 s).
pub fn parse_psi(b: &[u8]) -> f32 {
    let Some(p) = b.windows(6).position(|w| w == b"avg10=") else {
        return 0.0;
    };
    let (a, n) = parse_u64(&b[p + 6..]);
    let f = if b.get(p + 6 + n) == Some(&b'.') {
        parse_u64(&b[p + 7 + n..]).0.min(99)
    } else {
        0
    };
    a as f32 + f as f32 / 100.0
}

/// /proc/swaps lists only zram devices (and at least one).
pub fn swaps_zram(t: &str) -> bool {
    let mut n = 0;
    for l in t.lines().skip(1) {
        let dev = l.split_ascii_whitespace().next().unwrap_or("");
        if !dev.starts_with("/dev/zram") {
            return false;
        }
        n += 1;
    }
    n > 0
}

/// Disk counters (sysfs `stat` / the fields after the name in /proc/diskstats): reads,
/// read sectors, writes, write sectors, io_ticks (ms busy).
pub type DiskCnt = [u64; 5];

pub fn parse_disk_fields(b: &[u8]) -> DiskCnt {
    let mut v = [0u64; 11];
    let mut i = 0;
    for x in v.iter_mut() {
        let (n, k) = parse_u64(&b[i.min(b.len())..]);
        *x = n;
        i += k;
    }
    [v[0], v[2], v[4], v[6], v[9]]
}

/// One /proc/diskstats line → (name, counters).
pub fn parse_diskstats_line(l: &str) -> Option<(&str, DiskCnt)> {
    let mut it = l.split_ascii_whitespace();
    it.next()?;
    it.next()?;
    let name = it.next()?;
    let rest = &l[l.find(name)? + name.len()..];
    Some((name, parse_disk_fields(rest.as_bytes())))
}

#[derive(Clone, Debug, PartialEq)]
pub struct MountEnt {
    pub mnt: String,
    pub dev: String,
    pub fstype: String,
}

/// mountinfo "\040" style escapes.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..b'8').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Real filesystems from /proc/self/mountinfo: block-device (or zfs / bcachefs)
/// backed, no loop devices or squashfs (snaps), no network filesystems (statvfs can
/// hang), each device once (bind mounts, btrfs subvolumes: the shortest path wins).
pub fn parse_mountinfo(text: &str) -> Vec<MountEnt> {
    let mut v: Vec<(String, MountEnt)> = Vec::new();
    for l in text.lines() {
        let Some((a, b)) = l.split_once(" - ") else {
            continue;
        };
        let f: Vec<&str> = a.split(' ').collect();
        let g: Vec<&str> = b.split(' ').collect();
        if f.len() < 5 || g.len() < 2 {
            continue;
        }
        let (fstype, src) = (g[0], g[1]);
        let real = (src.starts_with("/dev/") && !src.starts_with("/dev/loop"))
            || fstype == "zfs"
            || fstype == "bcachefs";
        if !real
            || matches!(
                fstype,
                "squashfs" | "iso9660" | "udf" | "nfs" | "nfs4" | "cifs"
            )
        {
            continue;
        }
        let e = MountEnt {
            mnt: unescape(f[4]),
            dev: unescape(src),
            fstype: fstype.to_string(),
        };
        let key = if fstype == "zfs" {
            e.dev.clone()
        } else {
            f[2].to_string()
        };
        match v.iter_mut().find(|(k, _)| *k == key) {
            Some((_, o)) => {
                if e.mnt.len() < o.mnt.len() {
                    *o = e;
                }
            }
            None => v.push((key, e)),
        }
    }
    // (mount order: / first)
    v.into_iter().map(|(_, e)| e).collect()
}

// ── slow sensors (helper thread) ────────────────────────────────────────────

const NSLOW: usize = 8;
static SLOW_FD: [AtomicI32; NSLOW] = [const { AtomicI32::new(-1) }; NSLOW];
/// millidegrees, i32::MIN = not read yet
static SLOW_T: [AtomicI32; NSLOW] = [const { AtomicI32::new(i32::MIN) }; NSLOW];
static SLOW_N: AtomicUsize = AtomicUsize::new(0);
/// read period in ms; 0 = idle (nothing visible needs them)
static SLOW_MS: AtomicU32 = AtomicU32::new(0);
/// futex word: bumped whenever SLOW_MS changes
static SLOW_GEN: AtomicU32 = AtomicU32::new(0);
static SLOW_UP: AtomicU32 = AtomicU32::new(0);

#[cfg(unix)]
fn futex(w: &AtomicU32, op: c_int, val: u32, ts: *const libc::timespec) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            w.as_ptr(),
            op | libc::FUTEX_PRIVATE_FLAG,
            val,
            ts,
        );
    }
}

#[cfg(unix)]
extern "C" fn slow_main(_: *mut libc::c_void) -> *mut libc::c_void {
    let mut buf = [0u8; 16];
    loop {
        let g = SLOW_GEN.load(Relaxed);
        let ms = SLOW_MS.load(Relaxed);
        if ms > 0 {
            for i in 0..SLOW_N.load(Relaxed) {
                let n = pread(SLOW_FD[i].load(Relaxed), &mut buf);
                let (v, k) = parse_u64(&buf[..n]);
                if k > 0 {
                    SLOW_T[i].store(v as i32, Relaxed);
                }
            }
        }
        let ts = libc::timespec {
            tv_sec: (ms / 1000) as _,
            tv_nsec: (ms % 1000) as i64 * 1_000_000,
        };
        futex(
            &SLOW_GEN,
            libc::FUTEX_WAIT,
            g,
            if ms > 0 { &ts } else { std::ptr::null() },
        );
    }
}

/// How often the helper thread reads the slow sensors (0 = not at all). Starts the
/// thread on first use (after the main thread blocked its signals: it inherits that).
pub fn slow_period(ms: u32) {
    if SLOW_N.load(Relaxed) == 0 || SLOW_MS.load(Relaxed) == ms {
        return;
    }
    SLOW_MS.store(ms, Relaxed);
    SLOW_GEN.fetch_add(1, Relaxed);
    if SLOW_UP.swap(1, Relaxed) == 0 {
        #[cfg(windows)]
        win::start_slow();
        #[cfg(unix)]
        unsafe {
            let mut a: libc::pthread_attr_t = std::mem::zeroed();
            let mut t: libc::pthread_t = std::mem::zeroed();
            libc::pthread_attr_init(&mut a);
            libc::pthread_attr_setstacksize(&mut a, 64 * 1024);
            libc::pthread_create(&mut t, &a, slow_main, std::ptr::null_mut());
            libc::pthread_attr_destroy(&mut a);
        }
    } else {
        #[cfg(windows)]
        win::wake_slow();
        #[cfg(unix)]
        futex(&SLOW_GEN, libc::FUTEX_WAKE, 1, std::ptr::null());
    }
}

#[cfg(unix)]
fn add_slow(path: &str) -> Option<usize> {
    let i = SLOW_N.load(Relaxed);
    let fd = open_ro(path);
    if i >= NSLOW || fd < 0 {
        return None;
    }
    SLOW_FD[i].store(fd, Relaxed);
    SLOW_N.store(i + 1, Relaxed);
    Some(i)
}

/// A slow sensor's last reading (°C).
pub fn slow_temp(i: usize) -> Option<f32> {
    let v = SLOW_T.get(i)?.load(Relaxed);
    (v != i32::MIN).then(|| v as f32 / 1000.0)
}

// ── disks ──────────────────────────────────────────────────────────────────

/// Counter snapshots kept per disk / for swap: rates over ~1 s at fast speeds (disk
/// rates swing wildly over shorter spans; a second is what a reader takes in anyway).
const SNAP: usize = 16;
const RATE_NS: i64 = 1_000_000_000;

#[derive(Clone)]
struct Snaps<const N: usize> {
    t: [i64; SNAP],
    v: [[u64; N]; SNAP],
    head: usize,
    len: usize,
}

impl<const N: usize> Snaps<N> {
    fn new() -> Self {
        Snaps {
            t: [0; SNAP],
            v: [[0; N]; SNAP],
            head: 0,
            len: 0,
        }
    }
    /// Per-second rates against the newest snapshot at least RATE_NS old (else the
    /// oldest), then remember `cur`.
    fn rates(&mut self, now: i64, cur: [u64; N], out: &mut [f32; N]) {
        if self.len > 0 {
            let mut pick = (self.head + SNAP - self.len) % SNAP;
            for k in 0..self.len {
                let j = (self.head + SNAP - 1 - k) % SNAP;
                if now - self.t[j] >= RATE_NS - RATE_NS / 25 {
                    pick = j;
                    break;
                }
            }
            let dt = (now - self.t[pick]) as f32 / 1e9;
            if dt > 0.0 {
                for k in 0..N {
                    out[k] = cur[k].saturating_sub(self.v[pick][k]) as f32 / dt;
                }
            }
        }
        self.t[self.head] = now;
        self.v[self.head] = cur;
        self.head = (self.head + 1) % SNAP;
        self.len = (self.len + 1).min(SNAP);
    }
}

pub struct Disk {
    pub name: String,
    pub model: String,
    pub size: u64,
    fd: c_int,
    snaps: Snaps<5>,
    /// counters at the last history point, and the latest
    step: (i64, DiskCnt),
    cur: (i64, DiskCnt),
    /// latest rates: read / write bytes/s, read / write IOPS, busy (0..1)
    pub rd: f32,
    pub wr: f32,
    pub ri: f32,
    pub wi: f32,
    pub busy: f32,
    pub h_rd: Ring<f32>,
    pub h_wr: Ring<f32>,
    /// helper-thread sensor of the drive's temperature
    pub temp: Option<usize>,
}

impl Disk {
    pub fn new(name: &str) -> Disk {
        let rd = |f: &str| crate::sys::read_str(&format!("/sys/block/{name}/{f}"));
        Disk {
            name: name.to_string(),
            model: rd("device/model")
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            size: rd("size").and_then(|s| s.parse().ok()).unwrap_or(0) * 512,
            fd: open_ro(&format!("/sys/block/{name}/stat")),
            snaps: Snaps::new(),
            step: (0, [0; 5]),
            cur: (0, [0; 5]),
            rd: 0.0,
            wr: 0.0,
            ri: 0.0,
            wi: 0.0,
            busy: 0.0,
            h_rd: Ring::new(),
            h_wr: Ring::new(),
            temp: None,
        }
    }

    fn update(&mut self, now: i64, c: DiskCnt) {
        let mut r = [0f32; 5];
        if self.cur.0 == 0 {
            self.step = (now, c);
        }
        self.cur = (now, c);
        self.snaps.rates(now, c, &mut r);
        self.ri = r[0];
        self.rd = r[1] * 512.0;
        self.wi = r[2];
        self.wr = r[3] * 512.0;
        self.busy = (r[4] / 1000.0).min(1.0);
    }

    /// Read / write bytes/s over the unfinished history second.
    pub fn part(&self) -> (f32, f32) {
        let dt = (self.cur.0 - self.step.0) as f32 / 1e9;
        if dt <= 0.0 {
            return (self.rd, self.wr);
        }
        let d = |k: usize| self.cur.1[k].saturating_sub(self.step.1[k]) as f32 * 512.0 / dt;
        (d(1), d(3))
    }

    fn push(&mut self, times: u32) {
        let (r, w) = self.part();
        for _ in 0..times {
            self.h_rd.push(r);
            self.h_wr.push(w);
        }
        self.step = self.cur;
    }
}

/// Whole physical disks: block devices with a `device` (not loop, dm, md, zram, ram)
/// and a size, in /proc/diskstats order.
#[cfg(unix)]
fn find_disks() -> Vec<Disk> {
    let ds = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let mut v = Vec::new();
    for l in ds.lines() {
        let Some((name, _)) = parse_diskstats_line(l) else {
            continue;
        };
        let p = format!("/sys/block/{name}");
        // (not optical / floppy drives, nor eMMC boot and RPMB areas)
        let skip = name.starts_with("sr")
            || name.starts_with("fd")
            || (name.starts_with("mmcblk") && (name.contains("boot") || name.contains("rpmb")));
        if !skip
            && std::path::Path::new(&format!("{p}/device")).exists()
            && crate::sys::read_int(&format!("{p}/size")).unwrap_or(0) > 0
        {
            v.push(Disk::new(name));
        }
    }
    v
}

#[cfg(unix)]
fn canon(p: &str) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(p).ok()
}

/// DIMM sensors (spd5118 / jc42) and drive sensors (nvme / drivetemp, matched to
/// the disks by their device): registered with the helper thread.
#[cfg(unix)]
fn find_slow(disks: &mut [Disk]) -> Vec<usize> {
    let mut dimms = Vec::new();
    for d in crate::sys::hwmon_dirs() {
        let name = crate::sys::read_str(&format!("{d}/name")).unwrap_or_default();
        let inp = format!("{d}/temp1_input");
        match name.as_str() {
            "spd5118" | "jc42" => dimms.extend(add_slow(&inp)),
            "nvme" | "drivetemp" => {
                let dev = canon(&format!("{d}/device"));
                if let Some(k) = disks.iter().position(|k| {
                    k.temp.is_none()
                        && dev.is_some()
                        && canon(&format!("/sys/block/{}/device", k.name)) == dev
                }) {
                    disks[k].temp = add_slow(&inp);
                }
            }
            _ => {}
        }
    }
    dimms
}

// ── filesystems ────────────────────────────────────────────────────────────

pub struct Fs {
    pub mnt: String,
    pub dev: String,
    pub fstype: String,
    path: CString,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

impl Fs {
    /// Use % as df shows it: used / (used + available), rounded up.
    pub fn pct(&self) -> u32 {
        let d = self.used + self.avail;
        if d == 0 {
            0
        } else {
            (self.used * 100).div_ceil(d) as u32
        }
    }
    #[cfg(unix)]
    fn stat(&mut self) {
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(self.path.as_ptr(), &mut s) } == 0 {
            let f = s.f_frsize as u64;
            self.total = s.f_blocks as u64 * f;
            self.used = (s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * f;
            self.avail = s.f_bavail as u64 * f;
        }
    }
}

// ── processes ──────────────────────────────────────────────────────────────

/// A process followed live: its statm stays open and is re-read with one pread.
pub struct Proc {
    pub pid: u32,
    /// resident and shared (file-backed + shmem) bytes, as statm reports them
    pub rss: u64,
    pub shared: u64,
    /// index into `Mem::groups`
    pub group: usize,
    fd: c_int,
}

/// All processes of one program (same name).
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub name: String,
    pub n: u32,
    /// estimated RAM of the program: the private memory of every process plus the
    /// largest shared part once (shared libraries and shared memory are mapped by
    /// each process but held in RAM once)
    pub mem: u64,
    /// the processes not followed live, as of the last full scan
    base_priv: u64,
    base_shared: u64,
    base_n: u32,
}

/// Resident and shared pages from a statm buffer.
fn parse_statm(b: &[u8]) -> (u64, u64) {
    let (_, k) = parse_u64(b);
    let (r, k2) = parse_u64(&b[k..]);
    (r, parse_u64(&b[k + k2..]).0)
}

/// Process `pid`'s statm, opened relative to /proc (`keep`: return the open fd).
#[cfg(unix)]
fn statm_open(dfd: c_int, pid: u32) -> c_int {
    let mut path = [0u8; 24];
    let mut f = crate::ui::F::new();
    f.int(pid, 0);
    f.str("/statm");
    f.push(0);
    path[..f.n].copy_from_slice(&f.s().as_bytes()[..f.n]);
    unsafe {
        libc::openat(
            dfd,
            path.as_ptr() as *const _,
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    }
}

/// (resident, shared) pages, or None if the process is gone.
#[cfg(unix)]
fn statm_read(fd: c_int) -> Option<(u64, u64)> {
    let mut b = [0u8; 128];
    let n = unsafe { libc::pread(fd, b.as_mut_ptr() as *mut _, b.len(), 0) };
    (n > 0).then(|| parse_statm(&b[..n as usize]))
}

/// A process's program: argv[0]'s file name (comm is often a thread's name, such as
/// node's "MainThread"), else comm.
#[cfg(unix)]
fn proc_name(pid: u32) -> String {
    let c = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let a0 = c.split(|&b| b == 0 || b == b' ').next().unwrap_or(&[]);
    let base = a0.rsplit(|&b| b == b'/').next().unwrap_or(&[]);
    let n = String::from_utf8_lossy(base);
    let n = n.trim_end_matches(':');
    if n.is_empty() {
        crate::sys::read_str(&format!("/proc/{pid}/comm")).unwrap_or_default()
    } else {
        n.to_string()
    }
}

/// Indices of the `k` biggest of `n` values, biggest first (insertion into a short
/// list: n ≈ 500, k ≈ 30).
fn top_k(n: usize, k: usize, v: &dyn Fn(usize) -> u64) -> Vec<usize> {
    let mut t: Vec<usize> = Vec::with_capacity(k + 1);
    for i in 0..n {
        let x = v(i);
        if t.len() < k || x > v(t[t.len() - 1]) {
            let at = t.iter().position(|&j| v(j) < x).unwrap_or(t.len());
            t.insert(at, i);
            t.truncate(k);
        }
    }
    t
}

/// Processes followed live (statm kept open): the biggest ones, which is where the
/// visible changes happen. Programs whose processes all sit below them change only
/// at full scans.
pub const NPROC: usize = 32;
/// Programs kept (the panel shows as many as fit).
pub const NGROUP: usize = 24;

// ── all of it ──────────────────────────────────────────────────────────────

pub struct Mem {
    meminfo_fd: c_int,
    vmstat_fd: c_int,
    psi_fd: c_int,
    mounts_fd: c_int,
    buf: Vec<u8>,
    pub mi: MemInfo,
    /// swap in / out bytes/s, memory pressure (PSI some avg10, %)
    pub swin: f32,
    pub swout: f32,
    pub psi: f32,
    pub has_psi: bool,
    /// all swap is zram (compressed RAM, not disk)
    pub zram: bool,
    swap_snaps: Snaps<2>,
    /// used, and used + cache (fractions of RAM), one point a second
    pub h_used: Ring<f32>,
    pub h_cache: Ring<f32>,
    pub disks: Vec<Disk>,
    pub fs: Vec<Fs>,
    /// the biggest processes, followed live, biggest first
    pub procs: Vec<Proc>,
    /// programs (processes grouped by name), biggest first
    pub groups: Vec<Group>,
    pub dimms: Vec<usize>,
    /// pids known to have no memory (kernel threads)
    skip: Vec<u32>,
    /// every process at the last scan: pid, resident, shared (bytes), name index
    all: Vec<(u32, u64, u64, u32)>,
    /// indices of the biggest `groups`, biggest first
    pub top: Vec<usize>,
    /// names by pid (sorted by pid), interned in `names`
    name_of: Vec<(u32, u32)>,
    names: Vec<String>,
    /// RAM in use and the followed processes' total at the last full scan
    scan_used: u64,
    scan_live: u64,
    checks: u32,
    scans: u32,
    hidden: u32,
    held: u32,
    #[cfg(unix)]
    proc_dir: *mut libc::DIR,
    #[cfg(windows)]
    w: win::State,
    page: u64,
    pub hist_n: u64,
    pub last_fs: i64,
    pub last_proc: i64,
    pub last_top: i64,
    pub last_vm: i64,
    /// processes were scanned at least once
    pub proc_ok: bool,
}

impl Mem {
    pub fn empty() -> Mem {
        Mem {
            meminfo_fd: -1,
            vmstat_fd: -1,
            psi_fd: -1,
            mounts_fd: -1,
            buf: Vec::new(),
            mi: MemInfo::default(),
            swin: 0.0,
            swout: 0.0,
            psi: 0.0,
            has_psi: false,
            zram: false,
            swap_snaps: Snaps::new(),
            h_used: Ring::new(),
            h_cache: Ring::new(),
            disks: Vec::new(),
            fs: Vec::new(),
            procs: Vec::new(),
            groups: Vec::new(),
            dimms: Vec::new(),
            skip: Vec::new(),
            all: Vec::new(),
            top: Vec::new(),
            name_of: Vec::new(),
            names: Vec::new(),
            scan_used: 0,
            scan_live: 0,
            checks: 0,
            scans: 0,
            hidden: 0,
            held: 0,
            #[cfg(unix)]
            proc_dir: std::ptr::null_mut(),
            #[cfg(windows)]
            w: win::State::new(),
            page: 4096,
            hist_n: 0,
            last_fs: i64::MIN / 2,
            last_proc: i64::MIN / 2,
            last_top: i64::MIN / 2,
            last_vm: i64::MIN / 2,
            proc_ok: false,
        }
    }

    #[cfg(unix)]
    pub fn new() -> Mem {
        let mut m = Mem::empty();
        m.meminfo_fd = open_ro("/proc/meminfo");
        m.vmstat_fd = open_ro("/proc/vmstat");
        m.psi_fd = open_ro("/proc/pressure/memory");
        m.mounts_fd = open_ro("/proc/self/mountinfo");
        m.buf = vec![0; 8192];
        m.has_psi = m.psi_fd >= 0 && pread(m.psi_fd, &mut m.buf) > 0;
        m.zram = swaps_zram(&std::fs::read_to_string("/proc/swaps").unwrap_or_default());
        m.page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1024) as u64;
        m.disks = find_disks();
        m.dimms = find_slow(&mut m.disks);
        m.read_mounts();
        m.sample(0, crate::term::mono(), true, false);
        m
    }

    #[cfg(unix)]
    fn read_mounts(&mut self) {
        let t = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        self.fs = parse_mountinfo(&t)
            .into_iter()
            .filter_map(|e| {
                let path = CString::new(e.mnt.as_str()).ok()?;
                let mut f = Fs {
                    mnt: e.mnt,
                    dev: e.dev,
                    fstype: e.fstype,
                    path,
                    total: 0,
                    used: 0,
                    avail: 0,
                };
                f.stat();
                (f.total > 0).then_some(f)
            })
            .collect();
    }

    /// Filesystems: statvfs each, the mount table again only if it changed (poll on
    /// mountinfo flags a change since the last look).
    #[cfg(unix)]
    pub fn refresh_fs(&mut self) {
        let mut p = libc::pollfd {
            fd: self.mounts_fd,
            events: libc::POLLPRI,
            revents: 0,
        };
        if self.mounts_fd >= 0
            && unsafe { libc::poll(&mut p, 1, 0) } > 0
            && p.revents & (libc::POLLPRI | libc::POLLERR) != 0
        {
            self.read_mounts();
        }
        for f in &mut self.fs {
            f.stat();
        }
    }

    /// One sample: memory and disks always (cheap; their histories keep going), swap
    /// rates and pressure when `vis` (`vm`: also re-read /proc/vmstat).
    pub fn sample(&mut self, push: u32, now: i64, vis: bool, vm: bool) {
        // hidden: every third sample is enough to keep the histories going (eco: each
        // costs ~20 µs with cold caches); the points in between repeat the next one
        self.hidden = if vis { 0 } else { self.hidden + 1 };
        self.held += push;
        if self.hidden % 3 != 1 && !vis {
            return;
        }
        let push = std::mem::take(&mut self.held);
        #[cfg(windows)]
        {
            let _ = vm;
            self.read_now(now);
        }
        #[cfg(unix)]
        {
            let n = pread(self.meminfo_fd, &mut self.buf);
            parse_meminfo(&self.buf[..n], &mut self.mi);
            let mut b = [0u8; 256];
            for d in &mut self.disks {
                let n = pread(d.fd, &mut b);
                if n > 0 {
                    d.update(now, parse_disk_fields(&b[..n]));
                }
            }
        }
        // (Windows: no swap-in / swap-out counts nor pressure; the page file's use is
        // read above)
        #[cfg(unix)]
        if vis && vm {
            self.last_vm = now;
            if self.mi.swap_total > 0 {
                let n = pread(self.vmstat_fd, &mut self.buf);
                let (i, o) = parse_vmstat(&self.buf[..n]);
                let mut r = [0f32; 2];
                self.swap_snaps.rates(now, [i, o], &mut r);
                (self.swin, self.swout) = (r[0] * self.page as f32, r[1] * self.page as f32);
            }
            if self.has_psi {
                let n = pread(self.psi_fd, &mut self.buf);
                self.psi = parse_psi(&self.buf[..n]);
            }
        }
        if push > 0 {
            let t = self.mi.total.max(1) as f32;
            let u = self.mi.used() as f32 / t;
            let c = (u + self.mi.cache() as f32 / t).min(1.0);
            for _ in 0..push {
                self.h_used.push(u);
                self.h_cache.push(c);
            }
            for d in &mut self.disks {
                d.push(push);
            }
            self.hist_n += 1;
        }
    }

    /// Drop what only the next full scan needs (every process's sizes, the names by
    /// pid): the overview scans rarely and keeps its memory small; the next scan
    /// reads the names again.
    #[cfg(unix)]
    pub fn shrink(&mut self) {
        if !self.proc_dir.is_null() {
            // (the listing's buffer is 32 KB: opened again for the next scan)
            unsafe { libc::closedir(self.proc_dir) };
            self.proc_dir = std::ptr::null_mut();
        }
        self.all = Vec::new();
        self.name_of = Vec::new();
        self.names = Vec::new();
        for g in &mut self.groups {
            g.name.shrink_to_fit();
        }
    }

    /// Full scan of /proc (~1.3 µs a process): every process's memory, grouped by
    /// program name; the `NPROC` biggest are then followed live (`refresh_procs`).
    /// Kernel threads (no memory) are remembered and skipped; that list is rebuilt
    /// every tenth scan in case their pids were reused. Names are read once per pid.
    #[cfg(unix)]
    pub fn scan_procs(&mut self) {
        if self.proc_dir.is_null() {
            self.proc_dir = unsafe { libc::opendir(c"/proc".as_ptr()) };
            if self.proc_dir.is_null() {
                return;
            }
        }
        self.scans += 1;
        self.last_proc = crate::term::mono();
        self.last_top = self.last_proc;
        let fresh = self.scans % 10 == 1;
        if fresh {
            self.skip.clear();
        }
        let dir = self.proc_dir;
        let dfd = unsafe {
            libc::rewinddir(dir);
            libc::dirfd(dir)
        };
        self.all.clear();
        let (mut sk, mut nk) = (0, 0);
        let mut name_of = Vec::with_capacity(self.name_of.len() + 8);
        let page = self.page;
        loop {
            let e = unsafe { libc::readdir(dir) };
            if e.is_null() {
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*e).d_name.as_ptr()) }.to_bytes();
            if name.is_empty() || !name.iter().all(u8::is_ascii_digit) || name.len() > 20 {
                continue;
            }
            let pid = parse_u64(name).0 as u32;
            // (/proc lists pids in increasing order, and so do `skip` and `name_of`)
            while sk < self.skip.len() && self.skip[sk] < pid {
                sk += 1;
            }
            if !fresh && self.skip.get(sk) == Some(&pid) {
                continue;
            }
            let fd = statm_open(dfd, pid);
            if fd < 0 {
                continue;
            }
            let st = statm_read(fd);
            unsafe { libc::close(fd) };
            let Some((rss, sh)) = st else {
                continue;
            };
            if rss == 0 {
                if fresh {
                    self.skip.push(pid);
                }
                continue;
            }
            while nk < self.name_of.len() && self.name_of[nk].0 < pid {
                nk += 1;
            }
            let ni = match self.name_of.get(nk) {
                Some(&(p, ni)) if p == pid => ni,
                _ => {
                    let n = proc_name(pid);
                    match self.names.iter().position(|x| *x == n) {
                        Some(i) => i as u32,
                        None => {
                            self.names.push(n);
                            (self.names.len() - 1) as u32
                        }
                    }
                }
            };
            name_of.push((pid, ni));
            self.all.push((pid, rss * page, sh.min(rss) * page, ni));
        }
        self.name_of = name_of;
        // names no process uses any more: drop them (and renumber) now and then
        if self.names.len() > 4 * self.name_of.len().max(64) {
            let mut used = vec![u32::MAX; self.names.len()];
            let mut names = Vec::new();
            for e in self
                .name_of
                .iter_mut()
                .map(|e| &mut e.1)
                .chain(self.all.iter_mut().map(|e| &mut e.3))
            {
                let i = *e as usize;
                if used[i] == u32::MAX {
                    used[i] = names.len() as u32;
                    names.push(std::mem::take(&mut self.names[i]));
                }
                *e = used[i];
            }
            self.names = names;
        }
        // the NPROC biggest, followed live (fds of those already followed are kept)
        let big = top_k(self.all.len(), NPROC, &|i| self.all[i].1);
        // the biggest get their name read again: a pid first seen before an exec
        // (a shell that became the program) would keep the shell's name
        for &i in &big {
            let (pid, ni) = (self.all[i].0, self.all[i].3);
            let n = proc_name(pid);
            if n != self.names[ni as usize] && !n.is_empty() {
                let k = match self.names.iter().position(|x| *x == n) {
                    Some(k) => k,
                    None => {
                        self.names.push(n);
                        self.names.len() - 1
                    }
                } as u32;
                self.all[i].3 = k;
                if let Ok(j) = self.name_of.binary_search_by_key(&pid, |e| e.0) {
                    self.name_of[j].1 = k;
                }
            }
        }
        let mut old = std::mem::take(&mut self.procs);
        for &(pid, rss, shared, ni) in big.iter().map(|&i| &self.all[i]) {
            let fd = match old.iter().position(|p| p.pid == pid) {
                Some(i) => old.swap_remove(i).fd,
                None => statm_open(dfd, pid),
            };
            if fd >= 0 {
                self.procs.push(Proc {
                    pid,
                    rss,
                    shared,
                    group: ni as usize,
                    fd,
                });
            }
        }
        for p in old {
            unsafe { libc::close(p.fd) };
        }
        // programs: every process by name; the live ones are added in `regroup`
        let mut gi = vec![u32::MAX; self.names.len()];
        self.groups.clear();
        for &(pid, rss, shared, ni) in &self.all {
            let k = ni as usize;
            if gi[k] == u32::MAX {
                gi[k] = self.groups.len() as u32;
                self.groups.push(Group {
                    name: self.names[k].clone(),
                    n: 0,
                    mem: 0,
                    base_priv: 0,
                    base_shared: 0,
                    base_n: 0,
                });
            }
            if !self.procs.iter().any(|p| p.pid == pid) {
                let g = &mut self.groups[gi[k] as usize];
                g.base_priv += rss - shared;
                g.base_shared = g.base_shared.max(shared);
                g.base_n += 1;
            }
        }
        for p in &mut self.procs {
            p.group = gi[p.group] as usize;
        }
        self.regroup();
        self.scan_used = self.mi.used();
        self.scan_live = self.procs.iter().map(|p| p.rss).sum();
        self.proc_ok = true;
    }

    /// Between full scans (eco): has a program appeared or grown that the followed
    /// processes don't show? Checks /proc's pid list against the known one (statm
    /// only for new pids: one with 64 MB or more counts), and whether RAM in use
    /// changed by more than the followed processes explain (2 % of RAM, ≥ 256 MB).
    #[cfg(unix)]
    pub fn need_scan(&mut self) -> bool {
        let live: u64 = self.procs.iter().map(|p| p.rss).sum();
        let d_used = self.mi.used() as i64 - self.scan_used as i64;
        let d_live = live as i64 - self.scan_live as i64;
        if (d_used - d_live).unsigned_abs() > (self.mi.total / 50).max(256 << 20) {
            return true;
        }
        // (the pid list costs ~100 µs: every third check)
        self.checks += 1;
        if self.proc_dir.is_null() || self.checks % 3 != 0 {
            return false;
        }
        let dir = self.proc_dir;
        let dfd = unsafe {
            libc::rewinddir(dir);
            libc::dirfd(dir)
        };
        let (mut nk, mut sk) = (0, 0);
        loop {
            let e = unsafe { libc::readdir(dir) };
            if e.is_null() {
                return false;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*e).d_name.as_ptr()) }.to_bytes();
            if name.is_empty() || !name.iter().all(u8::is_ascii_digit) || name.len() > 20 {
                continue;
            }
            let pid = parse_u64(name).0 as u32;
            while nk < self.name_of.len() && self.name_of[nk].0 < pid {
                nk += 1;
            }
            while sk < self.skip.len() && self.skip[sk] < pid {
                sk += 1;
            }
            if self.name_of.get(nk).map(|e| e.0) == Some(pid) || self.skip.get(sk) == Some(&pid) {
                continue;
            }
            let fd = statm_open(dfd, pid);
            if fd < 0 {
                continue;
            }
            let st = statm_read(fd);
            unsafe { libc::close(fd) };
            if st.is_some_and(|(r, _)| r * self.page >= 64 << 20) {
                return true;
            }
        }
    }

    /// Re-read the processes followed live (one pread each, ~1 µs); the ones that
    /// ended are dropped.
    #[cfg(unix)]
    pub fn refresh_procs(&mut self) {
        let page = self.page;
        self.procs.retain_mut(|p| match statm_read(p.fd) {
            Some((r, s)) if r > 0 => {
                p.rss = r * page;
                p.shared = s.min(r) * page;
                true
            }
            _ => {
                unsafe { libc::close(p.fd) };
                false
            }
        });
        crate::sys::isort(&mut self.procs, &|a, b| a.rss > b.rss);
        self.regroup();
    }

    /// Program totals from the last scan's other processes and the live ones;
    /// the biggest `NGROUP`, biggest first.
    fn regroup(&mut self) {
        let mut sh: Vec<u64> = self.groups.iter().map(|g| g.base_shared).collect();
        for g in self.groups.iter_mut() {
            g.mem = g.base_priv;
            g.n = g.base_n;
        }
        for p in &self.procs {
            if let Some(g) = self.groups.get_mut(p.group) {
                g.mem += p.rss - p.shared;
                g.n += 1;
                sh[p.group] = sh[p.group].max(p.shared);
            }
        }
        for (g, s) in self.groups.iter_mut().zip(sh) {
            g.mem += s;
        }
        // (groups stay in index order for the lookups above: a sorted view)
        self.top = top_k(self.groups.len(), NGROUP, &|i| self.groups[i].mem);
    }
}

#[cfg(test)]
impl Mem {
    /// A fixed machine for layout and drawing tests.
    pub fn fake(ndisk: usize, nfs: usize, nproc: usize) -> Mem {
        let mut m = Mem::empty();
        let g = 1u64 << 30;
        m.mi = MemInfo {
            total: 30 * g,
            free: 5 * g,
            avail: 25 * g,
            buffers: g / 4,
            cached: 20 * g,
            sreclaim: g,
            shmem: g,
            swap_total: 8 * g,
            swap_free: 7 * g,
        };
        m.has_psi = true;
        for k in 0..ndisk {
            let mut d = Disk::new("none");
            d.name = format!("nvme{k}n1");
            d.model = "KLEVV CRAS C925G 2TB".into();
            d.size = 2_000_398_934_016;
            d.rd = 12e6 * k as f32;
            d.wr = 3e6;
            d.busy = 0.03;
            for i in 0..300 {
                d.h_rd.push((i % 17) as f32 * 1e6);
                d.h_wr.push((i % 5) as f32 * 1e5);
            }
            m.disks.push(d);
        }
        for k in 0..nfs {
            m.fs.push(Fs {
                mnt: if k == 0 {
                    "/".into()
                } else {
                    format!("/mnt/data{k}")
                },
                dev: "/dev/nvme0n1p2".into(),
                fstype: "ext4".into(),
                path: CString::new("/").unwrap(),
                total: 1000 * g,
                used: 90 * g * (k as u64 + 1),
                avail: 800 * g,
            });
        }
        for k in 0..nproc {
            let rss = (2 * g) >> k.min(30);
            m.groups.push(Group {
                name: format!("program-{k}"),
                n: 1 + (k % 3) as u32,
                mem: rss,
                base_priv: rss,
                base_shared: 0,
                base_n: 1 + (k % 3) as u32,
            });
            if k < NPROC {
                m.procs.push(Proc {
                    pid: 1000 + k as u32,
                    rss,
                    shared: 0,
                    group: k,
                    fd: -1,
                });
            }
        }
        m.top = (0..nproc.min(NGROUP)).collect();
        m.proc_ok = true;
        for i in 0..300 {
            m.h_used.push(0.15 + (i % 7) as f32 / 100.0);
            m.h_cache.push(0.8);
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_matches_free() {
        let t = b"MemTotal:       31407440 kB\nMemFree:         5575712 kB\nMemAvailable:   26533168 kB\nBuffers:          123456 kB\nCached:         21000000 kB\nSwapCached:        12345 kB\nShmem:           1156892 kB\nSReclaimable:     900000 kB\nSwapTotal:       8388604 kB\nSwapFree:        8154164 kB\n";
        let mut m = MemInfo::default();
        parse_meminfo(t, &mut m);
        assert_eq!(m.total, 31407440 * 1024);
        assert_eq!(m.used(), (31407440 - 26533168) * 1024);
        assert_eq!(m.cache(), (123456 + 21000000 + 900000) * 1024);
        assert_eq!(m.shmem, 1156892 * 1024);
        assert_eq!(m.swap_used(), (8388604 - 8154164) * 1024);
        // no MemAvailable (old kernels)
        let mut o = MemInfo::default();
        parse_meminfo(
            b"MemTotal: 100 kB\nMemFree: 10 kB\nBuffers: 5 kB\nCached: 20 kB\n",
            &mut o,
        );
        assert_eq!(o.avail, 35 * 1024);
    }

    #[test]
    fn vmstat_and_psi() {
        let t = b"nr_free_pages 1234\npgpgin 99\npswpin 42\npswpout 7\npgalloc 1\n";
        assert_eq!(parse_vmstat(t), (42, 7));
        assert_eq!(parse_vmstat(b"nr_free_pages 1\n"), (0, 0));
        let p = b"some avg10=1.25 avg60=0.03 avg300=0.06 total=25256055\nfull avg10=0.00 avg60=0.03 avg300=0.06 total=1\n";
        assert_eq!(parse_psi(p), 1.25);
        assert_eq!(parse_psi(b"some avg10=0.00 avg60"), 0.0);
        assert_eq!(parse_psi(b""), 0.0);
    }

    #[test]
    fn diskstats_lines() {
        let l = " 259       0 nvme0n1 1234 10 56789 300 4321 20 98765 400 0 777 888 0 0 0 0 5 6";
        let (n, c) = parse_diskstats_line(l).unwrap();
        assert_eq!(n, "nvme0n1");
        assert_eq!(c, [1234, 56789, 4321, 98765, 777]);
        // sysfs stat: same fields without major / minor / name
        let s = b"    1234       10    56789      300     4321       20    98765      400        0      777      888\n";
        assert_eq!(parse_disk_fields(s), c);
        assert!(parse_diskstats_line("").is_none());
    }

    #[test]
    fn rates_over_a_window() {
        let mut s: Snaps<1> = Snaps::new();
        let mut r = [0f32; 1];
        let ms = 1_000_000i64;
        for k in 0..20i64 {
            s.rates(k * 100 * ms, [(k * 1000) as u64], &mut r);
        }
        // 10 000 / s, measured against the snapshot 0.5 s back
        assert!((r[0] - 10_000.0).abs() < 1.0, "{}", r[0]);
        let mut d = Disk::new("none");
        d.update(1_000_000_000, [0, 0, 0, 0, 0]);
        d.update(2_000_000_000, [10, 2048, 5, 4096, 250]);
        assert_eq!(
            (d.rd, d.wr, d.ri, d.wi),
            (1_048_576.0, 2_097_152.0, 10.0, 5.0)
        );
        assert!((d.busy - 0.25).abs() < 1e-6);
        assert_eq!(d.part(), (1_048_576.0, 2_097_152.0));
        d.push(1);
        assert_eq!(d.h_rd.back(0), 1_048_576.0);
    }

    #[test]
    fn mountinfo_keeps_real_filesystems_once() {
        let t = "\
22 1 252:0 / / rw,relatime shared:1 - ext4 /dev/mapper/ubuntu--vg-ubuntu--lv rw
23 22 0:5 / /dev rw shared:2 - devtmpfs udev rw
24 22 0:22 / /run rw - tmpfs tmpfs rw
30 22 259:2 / /boot rw shared:30 - ext4 /dev/nvme0n1p2 rw
31 30 259:1 / /boot/efi rw shared:31 - vfat /dev/nvme0n1p1 rw
40 22 7:0 / /snap/core/1 ro - squashfs /dev/loop0 ro
41 22 0:40 / /sys/firmware/efi/efivars rw - efivarfs efivarfs rw
42 22 0:41 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw
43 22 252:0 /home /srv/bind rw - ext4 /dev/mapper/ubuntu--vg-ubuntu--lv rw
44 22 0:50 / /mnt/my\\040disk rw - ext4 /dev/sdb1 rw
45 22 0:51 / /net rw - nfs4 /dev/fake rw
46 22 0:52 / /tank rw - zfs tank rw
";
        let v = parse_mountinfo(t);
        let m: Vec<&str> = v.iter().map(|e| e.mnt.as_str()).collect();
        assert_eq!(m, ["/", "/boot", "/boot/efi", "/mnt/my disk", "/tank"]);
        assert_eq!(v[0].fstype, "ext4");
        assert_eq!(v[2].dev, "/dev/nvme0n1p1");
    }

    #[test]
    fn df_percent_rounds_up() {
        let f = Fs {
            mnt: "/".into(),
            dev: String::new(),
            fstype: String::new(),
            path: CString::new("/").unwrap(),
            total: 1000,
            used: 91,
            avail: 900,
        };
        assert_eq!(f.pct(), 10);
    }
}

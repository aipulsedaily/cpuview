//! Memory & storage on Windows (the Windows half of mem.rs): RAM with its standby and
//! modified lists and the page file (system calls of ~1 µs), the physical disks'
//! counters (IOCTL_DISK_PERFORMANCE on \\.\PhysicalDriveN, opened once: ~1–8 µs a
//! read), fixed volumes (GetDiskFreeSpaceEx, ~10–70 µs each), the processes' memory
//! (the basic process list, ~0.1–0.4 ms, and process handles kept open: ~1 µs a
//! query), and NVMe temperatures for the helper thread (~0.2 ms of wall time each).

use super::*;
use crate::sys::isort;
use crate::win::{close, fd2h, h2fd, prog_name, wide, NtQuerySystemInformation, ProcList};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS};
use windows_sys::Win32::System::Ioctl::*;
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX2};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{OpenProcess, WaitOnAddress, WakeByAddressSingle, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::System::IO::DeviceIoControl;

const PAGE: u64 = 4096;

/// What the Windows sources keep between samples.
pub struct State {
    list: ProcList,
    /// processes seen: pid, identity, handle (as an fd; -1: can't be opened, its
    /// numbers come from the full list), sorted by pid
    open: Vec<(u32, u64, i32)>,
    /// memory of the processes that can't be opened (pid, resident, shared), from
    /// the last full list, and how many scans ago that was
    denied: Vec<(u32, u64, u64)>,
    since_full: u32,
    /// eco: no full list after the first (it costs milliseconds on a busy desktop)
    eco: bool,
    /// logical drives at the last volume listing
    drives: u32,
}

impl State {
    pub fn new() -> State {
        State {
            list: ProcList::new(),
            open: Vec::new(),
            denied: Vec::new(),
            since_full: u32::MAX,
            eco: false,
            drives: 0,
        }
    }
}

/// RAM (Task Manager's "available" and "cached": standby + modified) and the page
/// files as swap.
fn meminfo(m: &mut MemInfo) {
    unsafe {
        let mut s: MEMORYSTATUSEX = std::mem::zeroed();
        s.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut s) != 0 {
            m.total = s.ullTotalPhys;
            m.avail = s.ullAvailPhys;
        }
        // SystemMemoryListInformation: zeroed, free, modified, modified-no-write, bad,
        // standby by priority (8), …
        let mut l = [0usize; 24];
        let mut got = 0u32;
        if NtQuerySystemInformation(80, l.as_mut_ptr() as *mut _, std::mem::size_of_val(&l) as u32, &mut got) >= 0 {
            let standby: usize = l[5..13].iter().sum();
            m.free = (l[0] + l[1]) as u64 * PAGE;
            m.cached = (standby + l[2] + l[3]) as u64 * PAGE;
        } else {
            m.free = m.avail;
        }
        // SystemPageFileInformation: per page file next, size, in use, peak (pages),
        // name
        let mut b = [0u64; 64];
        if NtQuerySystemInformation(18, b.as_mut_ptr() as *mut _, std::mem::size_of_val(&b) as u32, &mut got) >= 0 {
            let p = b.as_ptr() as *const u8;
            let (mut o, mut tot, mut used) = (0usize, 0u64, 0u64);
            while got > 0 && o + 16 <= got as usize {
                let w = |k: usize| *(p.add(o + 4 * k) as *const u32) as u64;
                tot += w(1);
                used += w(2);
                if w(0) == 0 {
                    break;
                }
                o += w(0) as usize;
            }
            m.swap_total = tot * PAGE;
            m.swap_free = tot.saturating_sub(used) * PAGE;
        }
    }
}

fn ioctl<T>(h: HANDLE, code: u32, inp: &[u8], out: &mut T) -> bool {
    let mut got = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            code,
            if inp.is_empty() { std::ptr::null() } else { inp.as_ptr() as *const _ },
            inp.len() as u32,
            out as *mut T as *mut _,
            std::mem::size_of::<T>() as u32,
            &mut got,
            std::ptr::null_mut(),
        ) != 0
    }
}

fn open_drive(i: usize) -> HANDLE {
    let p = wide(&format!("\\\\.\\PhysicalDrive{i}"));
    // (no access rights: enough for the statistics, no administrator needed)
    unsafe {
        CreateFileW(
            p.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    }
}

/// A disk's counters as Linux's diskstats has them: reads, read sectors, writes,
/// written sectors, ms busy.
fn disk_counters(fd: c_int) -> Option<DiskCnt> {
    let mut p: DISK_PERFORMANCE = unsafe { std::mem::zeroed() };
    if !ioctl(fd2h(fd), IOCTL_DISK_PERFORMANCE, &[], &mut p) {
        return None;
    }
    let busy = (p.QueryTime - p.IdleTime).max(0) as u64 / 10_000;
    Some([
        p.ReadCount as u64,
        p.BytesRead as u64 / 512,
        p.WriteCount as u64,
        p.BytesWritten as u64 / 512,
        busy,
    ])
}

/// A drive's temperature (°C) as it reports it (NVMe; SATA drives mostly don't).
fn drive_temp(fd: c_int) -> Option<i32> {
    let mut q = [0u8; 12];
    q[..4].copy_from_slice(&(StorageDeviceTemperatureProperty as u32).to_le_bytes());
    let mut d: STORAGE_TEMPERATURE_DATA_DESCRIPTOR = unsafe { std::mem::zeroed() };
    if !ioctl(fd2h(fd), IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut d) || d.InfoCount == 0 {
        return None;
    }
    let t = d.TemperatureInfo[0].Temperature as i32;
    (t > -100 && t < 200).then_some(t)
}

/// The drive's vendor and product as it names itself.
fn drive_model(h: HANDLE) -> String {
    let mut out = [0u64; 128];
    let q = [0u8; 12]; // StorageDeviceProperty, PropertyStandardQuery
    if !ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out) {
        return String::new();
    }
    let b = unsafe { std::slice::from_raw_parts(out.as_ptr() as *const u8, 1024) };
    let d = unsafe { &*(out.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    let at = |o: u32| -> String {
        let o = o as usize;
        if o == 0 || o >= b.len() {
            return String::new();
        }
        let n = b[o..].iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf8_lossy(&b[o..o + n]).trim().to_string()
    };
    let (v, p) = (at(d.VendorIdOffset), at(d.ProductIdOffset));
    let m = if v.is_empty() || p.starts_with(&v) { p } else { v + " " + &p };
    m.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Physical disks, numbered as Disk Management numbers them (disk0, disk1 …); NVMe
/// drives that report a temperature get a helper-thread sensor on a handle of its own.
pub(super) fn find_disks() -> Vec<Disk> {
    let mut v = Vec::new();
    let mut miss = 0;
    for i in 0..64 {
        let h = open_drive(i);
        if h == INVALID_HANDLE_VALUE {
            miss += 1;
            if miss > 8 {
                break;
            }
            continue;
        }
        miss = 0;
        let mut g: DISK_GEOMETRY_EX = unsafe { std::mem::zeroed() };
        let size = if ioctl(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut g) { g.DiskSize as u64 } else { 0 };
        if size == 0 || disk_counters(h2fd(h)).is_none() {
            close(h);
            continue;
        }
        let mut d = Disk::new(&format!("disk{i}"));
        d.model = drive_model(h);
        d.size = size;
        d.fd = h2fd(h);
        if drive_temp(d.fd).is_some() {
            let t = open_drive(i);
            d.temp = add_slow(h2fd(t));
            if d.temp.is_none() {
                close(t);
            }
        }
        v.push(d);
    }
    v
}

fn add_slow(fd: c_int) -> Option<usize> {
    let i = SLOW_N.load(Relaxed);
    if i >= NSLOW || fd < 0 {
        return None;
    }
    SLOW_FD[i].store(fd, Relaxed);
    SLOW_N.store(i + 1, Relaxed);
    Some(i)
}

fn slow_main() {
    loop {
        let g = SLOW_GEN.load(Relaxed);
        let ms = SLOW_MS.load(Relaxed);
        if ms > 0 {
            for i in 0..SLOW_N.load(Relaxed) {
                if let Some(t) = drive_temp(SLOW_FD[i].load(Relaxed)) {
                    SLOW_T[i].store(t * 1000, Relaxed);
                }
            }
        }
        unsafe {
            WaitOnAddress(
                SLOW_GEN.as_ptr() as *const _,
                &g as *const u32 as *const _,
                4,
                if ms > 0 { ms } else { INFINITE },
            )
        };
    }
}

pub(super) fn start_slow() {
    let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(slow_main);
}

pub(super) fn wake_slow() {
    unsafe { WakeByAddressSingle(SLOW_GEN.as_ptr() as *const _) };
}

impl Fs {
    fn stat(&mut self) {
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExA(self.path.as_ptr() as *const u8, &mut avail, &mut total, &mut free) } != 0 {
            self.total = total;
            self.used = total.saturating_sub(free);
            self.avail = avail;
        }
    }
}

impl Mem {
    pub fn new() -> Mem {
        let mut m = Mem::empty();
        // (a drive without media must not pop up a dialog)
        unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) };
        m.buf = vec![0; 256];
        m.disks = find_disks();
        m.read_mounts();
        m.sample(0, crate::term::mono(), true, false);
        m
    }

    /// The speed is eco: no full process list after the first.
    pub fn set_eco(&mut self, eco: bool) {
        self.w.eco = eco;
    }

    /// Volumes with a drive letter on local disks (fixed and removable: network
    /// drives can hang, as on Linux).
    fn read_mounts(&mut self) {
        let mask = unsafe { GetLogicalDrives() };
        self.w.drives = mask;
        self.fs.clear();
        for k in 0..26u8 {
            if mask >> k & 1 == 0 {
                continue;
            }
            let root = [b'A' + k, b':', b'\\', 0];
            let kind = unsafe { GetDriveTypeA(root.as_ptr()) };
            if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE && kind != DRIVE_RAMDISK {
                continue;
            }
            let (mut label, mut fsn) = ([0u8; 64], [0u8; 32]);
            let ok = unsafe {
                GetVolumeInformationA(
                    root.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fsn.as_mut_ptr(),
                    fsn.len() as u32,
                )
            } != 0;
            if !ok {
                continue;
            }
            let z = |b: &[u8]| String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(0)]).to_string();
            let label = z(&label);
            let mut f = Fs {
                mnt: String::from_utf8_lossy(&root[..2]).to_string(),
                // (Explorer's name for a volume without a label)
                dev: if label.is_empty() { "Local Disk".into() } else { label },
                fstype: z(&fsn),
                path: CString::new(&root[..3]).unwrap_or_default(),
                total: 0,
                used: 0,
                avail: 0,
            };
            f.stat();
            if f.total > 0 {
                self.fs.push(f);
            }
        }
    }

    /// Volumes: free space each, the list again when a drive letter came or went.
    pub fn refresh_fs(&mut self) {
        if unsafe { GetLogicalDrives() } != self.w.drives {
            self.read_mounts();
        }
        for f in &mut self.fs {
            f.stat();
        }
    }

    /// RAM, page file and disks for `sample`.
    pub(super) fn read_now(&mut self, now: i64) {
        meminfo(&mut self.mi);
        for d in &mut self.disks {
            if let Some(c) = disk_counters(d.fd) {
                d.update(now, c);
            }
        }
    }

    /// Drop the scans' scratch memory (the overview scans rarely); the handles and
    /// names stay (they are what the next scan starts from).
    pub fn shrink(&mut self) {
        self.w.list.release();
        self.all = Vec::new();
        for g in &mut self.groups {
            g.name.shrink_to_fit();
        }
    }

    /// Resident and shared (resident but not private) bytes of an open process.
    fn query(fd: c_int) -> Option<(u64, u64)> {
        let mut c: PROCESS_MEMORY_COUNTERS_EX2 = unsafe { std::mem::zeroed() };
        let ok = unsafe { K32GetProcessMemoryInfo(fd2h(fd), &mut c as *mut _ as *mut _, std::mem::size_of_val(&c) as u32) };
        if ok == 0 {
            return None;
        }
        let rss = c.WorkingSetSize as u64;
        Some((rss, rss.saturating_sub(c.PrivateWorkingSetSize as u64)))
    }

    /// Full scan: every process from the basic list, its memory through a handle kept
    /// open (opened once per process); the processes that can't be opened (other
    /// users' services when not elevated) from the full list, read again every
    /// tenth scan, not in eco (it costs milliseconds on a busy desktop). Then as on
    /// Linux: grouped by program, the `NPROC` biggest followed live.
    pub fn scan_procs(&mut self) {
        self.w.since_full = self.w.since_full.saturating_add(1);
        let full = (self.w.since_full >= 10 && (!self.w.eco || !self.proc_ok)) || self.w.list.no_basic;
        if !self.w.list.read(full) {
            return;
        }
        if full {
            self.w.since_full = 0;
        }
        self.scans += 1;
        self.last_proc = crate::term::mono();
        self.last_top = self.last_proc;
        self.all.clear();
        let mut seen: Vec<(u32, u64, i32)> = Vec::with_capacity(self.w.open.len() + 8);
        let mut names: Vec<(u32, String)> = Vec::new();
        let mut denied = Vec::new();
        let (open, old_denied) = (&self.w.open, &self.w.denied);
        self.w.list.each(&mut |e| {
            if e.pid == 0 {
                return;
            }
            // the handle of this process (same pid and identity), else a new one
            let fd = match open.binary_search_by_key(&e.pid, |x| x.0) {
                Ok(i) if open[i].1 == e.key => open[i].2,
                _ => {
                    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.pid) };
                    names.push((e.pid, prog_name(e.name, e.pid)));
                    h2fd(h)
                }
            };
            seen.push((e.pid, e.key, fd));
            let m = if fd >= 0 { Self::query(fd) } else { None };
            let m = m.or_else(|| {
                let s = e.full?;
                Some((s.ws as u64, (s.ws as u64).saturating_sub(s.ws_private as u64)))
            });
            let m = m.or_else(|| old_denied.iter().find(|d| d.0 == e.pid).map(|d| (d.1, d.2)));
            if let Some((rss, sh)) = m {
                if fd < 0 {
                    denied.push((e.pid, rss, sh));
                }
                if rss > 0 {
                    self.all.push((e.pid, rss, sh, u32::MAX));
                }
            }
        });
        isort(&mut seen, &|a, b| a.0 < b.0);
        // handles of processes that ended
        for &(pid, key, fd) in &self.w.open {
            if fd >= 0 && seen.binary_search_by_key(&pid, |x| x.0).map_or(true, |i| seen[i].1 != key) {
                close(fd2h(fd));
            }
        }
        self.w.open = seen;
        if full || !denied.is_empty() {
            self.w.denied = denied;
        }
        // names: interned, by pid (read once per process)
        for (pid, n) in names {
            let ni = match self.names.iter().position(|x| *x == n) {
                Some(i) => i as u32,
                None => {
                    self.names.push(n);
                    (self.names.len() - 1) as u32
                }
            };
            match self.name_of.binary_search_by_key(&pid, |e| e.0) {
                Ok(j) => self.name_of[j].1 = ni,
                Err(j) => self.name_of.insert(j, (pid, ni)),
            }
        }
        self.name_of.retain(|e| self.w.open.binary_search_by_key(&e.0, |x| x.0).is_ok());
        for a in &mut self.all {
            if let Ok(j) = self.name_of.binary_search_by_key(&a.0, |e| e.0) {
                a.3 = self.name_of[j].1;
            }
        }
        self.all.retain(|a| a.3 != u32::MAX);
        if self.names.len() > 4 * self.name_of.len().max(64) {
            let mut used = vec![u32::MAX; self.names.len()];
            let mut names = Vec::new();
            for e in self.name_of.iter_mut().map(|e| &mut e.1).chain(self.all.iter_mut().map(|e| &mut e.3)) {
                let i = *e as usize;
                if used[i] == u32::MAX {
                    used[i] = names.len() as u32;
                    names.push(std::mem::take(&mut self.names[i]));
                }
                *e = used[i];
            }
            self.names = names;
        }
        // the NPROC biggest, followed live (their handles: owned by `open`)
        let big = top_k(self.all.len(), NPROC, &|i| self.all[i].1);
        self.procs.clear();
        for &(pid, rss, shared, ni) in big.iter().map(|&i| &self.all[i]) {
            let fd = self.w.open.binary_search_by_key(&pid, |x| x.0).map_or(-1, |i| self.w.open[i].2);
            self.procs.push(Proc {
                pid,
                rss,
                shared,
                group: ni as usize,
                fd,
            });
        }
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

    /// Between full scans: RAM in use changed by more than the followed processes
    /// explain, or (every third check) a process of 64 MB or more appeared.
    pub fn need_scan(&mut self) -> bool {
        let live: u64 = self.procs.iter().map(|p| p.rss).sum();
        let d_used = self.mi.used() as i64 - self.scan_used as i64;
        let d_live = live as i64 - self.scan_live as i64;
        if (d_used - d_live).unsigned_abs() > (self.mi.total / 50).max(256 << 20) {
            return true;
        }
        self.checks += 1;
        if self.checks % 3 != 0 || self.w.list.no_basic || !self.w.list.read(false) {
            return false;
        }
        let mut big = false;
        let open = &self.w.open;
        self.w.list.each(&mut |e| {
            if big || open.binary_search_by_key(&e.pid, |x| x.0).is_ok_and(|i| open[i].1 == e.key) {
                return;
            }
            let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.pid) };
            if !h.is_null() {
                big = Self::query(h2fd(h)).is_some_and(|(r, _)| r >= 64 << 20);
                close(h);
            }
        });
        big
    }

    /// Re-read the processes followed live (one query each); the ones that ended
    /// are dropped (their handles belong to the scan's list), the ones that can't be
    /// opened keep the last full list's numbers.
    pub fn refresh_procs(&mut self) {
        self.procs.retain_mut(|p| {
            if p.fd < 0 {
                return true;
            }
            match Self::query(p.fd) {
                Some((r, s)) if r > 0 => {
                    p.rss = r;
                    p.shared = s.min(r);
                    true
                }
                _ => false,
            }
        });
        isort(&mut self.procs, &|a, b| a.rss > b.rss);
        self.regroup();
    }
}

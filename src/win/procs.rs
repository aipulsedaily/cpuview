//! Processes on Windows (the Windows half of procs.rs). A full scan is one read of the
//! system's process list (CPU times, working set, I/O, priority and every thread's
//! state of every process: ~5 ms with 4 000 threads, 16 ms with 15 000): when the page
//! is entered, then (not in eco: no wakeup there may cost milliseconds) every
//! FULL_EVERY scans. The scans in between read the basic list (pids and names,
//! ~0.1–0.4 ms) for new and exited processes; every process this user may open keeps a
//! handle, and between scans a few dozen of them have their times and memory read
//! each sample (~2–5 µs each), round robin, so the order and totals stay current at a
//! bounded cost per sample. Names and command lines are read once per process, users
//! from its token (both only where Windows lets this user open the process: other
//! users' services show their name and numbers from the full scans only).
//!
//! Units as on Linux: `start` and `ticks` count `hz` ticks (100 ns here), `start`
//! from boot.

use super::*;
use crate::win::{close, fd2h, from_wide, h2fd, is_exe, prog_name, ProcList, Spi};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, FILETIME, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, LookupAccountSidW, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, GetTickCount64};
use windows_sys::Win32::System::Threading::*;

windows_link::link!("ntdll.dll" "system" fn NtQueryInformationProcess(h: HANDLE, class: i32, info: *mut core::ffi::c_void, len: u32, ret: *mut u32) -> i32);
windows_link::link!("ntdll.dll" "system" fn NtSuspendProcess(h: HANDLE) -> i32);
windows_link::link!("ntdll.dll" "system" fn NtResumeProcess(h: HANDLE) -> i32);
windows_link::link!("kernel32.dll" "system" fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32);

pub const HZ: f32 = 1e7;
/// full scans: one in this many (not in eco)
const FULL_EVERY: u32 = 30;
/// processes whose times are read per sample between scans
const CHUNK: usize = 24;

/// What the Windows source keeps: the list buffer, the boot time (FILETIME), users'
/// SIDs by uid (0 is SYSTEM, also for services whose token can't be read), and a
/// buffer for command lines.
pub struct State {
    list: ProcList,
    boot: i64,
    sids: Vec<Vec<u8>>,
    cbuf: Vec<u64>,
    /// processes seen: pid, identity in the basic list (0: not known yet), start,
    /// handle (as an fd; -1: can't be opened), when its times were last read; by pid
    hs: Vec<(u32, u64, u64, i32, i64)>,
    /// the next of them to read between scans, and when that last ran
    rr: usize,
    rr_t: i64,
    /// tracked processes: their last read
    tr_t: i64,
    /// eco (no full scans after the first), scans since the last full one
    eco: bool,
    light: u32,
}

impl State {
    /// Close the handles (leaving the page).
    pub fn clear(&mut self) {
        for h in self.hs.drain(..) {
            close(fd2h(h.3));
        }
    }
}

fn ft(f: &FILETIME) -> i64 {
    (f.dwHighDateTime as i64) << 32 | f.dwLowDateTime as i64
}

impl State {
    pub fn new() -> State {
        let mut now: FILETIME = unsafe { std::mem::zeroed() };
        unsafe { GetSystemTimeAsFileTime(&mut now) };
        State {
            list: ProcList::new(),
            boot: ft(&now) - unsafe { GetTickCount64() } as i64 * 10_000,
            // S-1-5-18 (LocalSystem)
            sids: vec![vec![1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]],
            cbuf: vec![0; 1024],
            hs: Vec::new(),
            rr: 0,
            rr_t: 0,
            tr_t: 0,
            eco: false,
            light: 0,
        }
    }
}

/// nice from a priority class's base priority (idle 4 … realtime 24).
fn nice_of(prio: i32) -> i8 {
    match prio {
        ..=4 => 19,
        5..=6 => 10,
        7..=8 => 0,
        9..=10 => -5,
        11..=15 => -10,
        _ => -20,
    }
}

/// The priority class for a nice value (what renice sets).
fn class_of(nice: i32) -> PROCESS_CREATION_FLAGS {
    match nice {
        15.. => IDLE_PRIORITY_CLASS,
        5..=14 => BELOW_NORMAL_PRIORITY_CLASS,
        -4..=4 => NORMAL_PRIORITY_CLASS,
        -9..=-5 => ABOVE_NORMAL_PRIORITY_CLASS,
        -19..=-10 => HIGH_PRIORITY_CLASS,
        _ => REALTIME_PRIORITY_CLASS,
    }
}

/// A process's state from its threads, as Linux letters: R (one is running or
/// ready), T (every thread suspended), Z (every thread ended), else S.
fn state_of(s: &Spi) -> u8 {
    // SYSTEM_THREAD_INFORMATION (0x50 bytes) follow the process record
    let base = s as *const Spi as *const u8;
    let (mut susp, mut dead) = (0, 0);
    for k in 0..s.threads as usize {
        let t = unsafe { base.add(0x100 + 0x50 * k) };
        let (st, why) = unsafe { (*(t.add(68) as *const u32), *(t.add(72) as *const u32)) };
        match st {
            1 | 2 | 3 | 7 => return b'R',
            4 => dead += 1,
            // waiting: Suspended / WrSuspended
            5 if why == 5 || why == 26 => susp += 1,
            _ => {}
        }
    }
    if s.threads > 0 && dead == s.threads {
        b'Z'
    } else if s.threads > 0 && susp + dead == s.threads {
        b'T'
    } else {
        b'S'
    }
}

fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> HANDLE {
    unsafe { OpenProcess(access, 0, pid) }
}

/// Process times: (start ticks after boot, kernel + user ticks, exited).
fn times(h: HANDLE, boot: i64) -> Option<(u64, u64, bool)> {
    let mut t: [FILETIME; 4] = unsafe { std::mem::zeroed() };
    if unsafe { GetProcessTimes(h, &mut t[0], &mut t[1], &mut t[2], &mut t[3]) } == 0 {
        return None;
    }
    Some(((ft(&t[0]) - boot).max(0) as u64, (ft(&t[2]) + ft(&t[3])) as u64, ft(&t[1]) != 0))
}

/// The command line (NtQueryInformationProcess, ProcessCommandLineInformation).
fn cmdline(h: HANDLE, buf: &mut [u64]) -> String {
    let mut got = 0u32;
    let len = std::mem::size_of_val(buf) as u32;
    if unsafe { NtQueryInformationProcess(h, 60, buf.as_mut_ptr() as *mut _, len, &mut got) } < 0 {
        return String::new();
    }
    // UNICODE_STRING: length (bytes), maximum, buffer
    let n = (buf[0] & 0xffff) as usize / 2;
    let p = buf[1] as *const u16;
    if p.is_null() {
        return String::new();
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// The arguments' byte offset in a command line (after argv[0], quoted or not).
fn args_at(c: &str) -> usize {
    let b = c.as_bytes();
    let end = if b.first() == Some(&b'"') {
        b[1..].iter().position(|&x| x == b'"').map_or(b.len(), |i| i + 2)
    } else {
        b.iter().position(|&x| x == b' ').unwrap_or(b.len())
    };
    (end + 1).min(b.len())
}

impl Procs {
    pub fn new(_root: &str) -> Procs {
        Procs {
            list: Vec::new(),
            tr: Vec::new(),
            dfd: -1,
            load_fd: -1,
            pids: Vec::new(),
            skip: Vec::new(),
            buf: Vec::new(),
            users: vec![(0, "SYSTEM".into())],
            gen: 0,
            scans: 0,
            hz: HZ,
            page: 4096,
            show_k: false,
            io_all: false,
            nk: 0,
            denied: 0,
            hide: 0,
            me: u32::MAX,
            load: [0.0; 3],
            scan_t: 0,
            cost: 0,
            last_track: i64::MIN / 2,
            ok: false,
            statm: true,
            w: State::new(),
        }
    }

    /// The uid (index of the SID) of a process's user, learning its name.
    fn user_of(&mut self, h: HANDLE) -> Option<u32> {
        let mut tok: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(h, TOKEN_QUERY, &mut tok) } == 0 {
            return None;
        }
        let mut b = [0u64; 32];
        let mut n = 0u32;
        let ok = unsafe { GetTokenInformation(tok, TokenUser, b.as_mut_ptr() as *mut _, 256, &mut n) };
        close(tok);
        if ok == 0 {
            return None;
        }
        let sid = unsafe { (*(b.as_ptr() as *const TOKEN_USER)).User.Sid } as *const u8;
        // (a SID: revision, sub-authority count, 6 bytes of authority, 4 per sub-authority)
        let len = 8 + 4 * unsafe { *sid.add(1) } as usize;
        let sid_b = unsafe { std::slice::from_raw_parts(sid, len) };
        if let Some(i) = self.w.sids.iter().position(|s| s == sid_b) {
            return Some(i as u32);
        }
        let (mut name, mut dom) = ([0u16; 128], [0u16; 128]);
        let (mut nn, mut dn, mut use_) = (128u32, 128u32, 0);
        let ok = unsafe {
            LookupAccountSidW(std::ptr::null(), sid as *mut _, name.as_mut_ptr(), &mut nn, dom.as_mut_ptr(), &mut dn, &mut use_)
        };
        let uid = self.w.sids.len() as u32;
        self.w.sids.push(sid_b.to_vec());
        let n = if ok != 0 { from_wide(&name) } else { "?".into() };
        self.users.push((uid, n.into()));
        Some(uid)
    }

    /// Name, command line and user of a process seen for the first time:
    /// (text, name length, arguments offset, uid).
    fn read_txt(&mut self, pid: u32, name: &[u16], session: u32) -> (Rc<str>, u8, u8, u32) {
        let mut s = String::new();
        clean(prog_name(name, pid).as_bytes(), 64, &mut s);
        let nlen = s.len() as u8;
        s.push('\0');
        let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION);
        let mut aoff = 0;
        let mut uid = if session == 0 { 0 } else { u32::MAX };
        if !h.is_null() {
            uid = self.user_of(h).unwrap_or(uid);
            if self.statm {
                let c = cmdline(h, &mut self.w.cbuf);
                let at = s.len();
                aoff = args_at(&c);
                clean(c.as_bytes(), CMD_MAX + s.len(), &mut s);
                mask_secrets(&mut s, at);
                aoff = if c.get(..aoff).is_some_and(|a| a.is_ascii()) { aoff.min(255) } else { 0 };
            }
            close(h);
        }
        let j = bsearch(&self.list, pid).unwrap_or_else(|j| j);
        let txt = (j.saturating_sub(1)..j + 2)
            .filter_map(|k| self.list.get(k))
            .find(|q| *q.txt == *s)
            .map_or_else(|| Rc::from(s), |q| q.txt.clone());
        (txt, nlen, aoff as u8, uid)
    }

    /// The speed is eco: no full scans after the first.
    pub fn set_eco(&mut self, eco: bool) {
        self.w.eco = eco;
    }

    /// A scan: a full one when the page was entered (and every FULL_EVERY scans, not
    /// in eco), else the basic list's new and exited processes. `keep_ms`: exited ones
    /// stay that long (fading).
    pub fn scan(&mut self, now: i64, keep_ms: u32) {
        let c0 = crate::term::cpu_ns();
        if self.ok && !self.w.list.no_basic && (self.w.eco || self.w.light + 1 < FULL_EVERY) {
            self.w.light += 1;
            self.light_scan(now);
        } else {
            self.w.light = 0;
            self.full_scan(now);
        }
        let ms = (now / 1_000_000) as u32 | 1;
        let gen = self.gen;
        for p in self.list.iter_mut() {
            if p.seen != gen && p.gone == 0 {
                p.gone = ms;
                p.cpu = 0.0;
            }
        }
        self.list
            .retain(|p| p.gone == 0 || (ms.wrapping_sub(p.gone) < keep_ms && keep_ms > 0));
        let list = &self.list;
        self.tr.retain_mut(|t| {
            let alive = bsearch(list, t.pid)
                .ok()
                .and_then(|j| list.get(j))
                .is_some_and(|p| p.start == t.start && p.gone == 0);
            if !alive {
                t.close();
            }
            alive
        });
        // handles: the processes listed (opened once), the others' closed
        let mut hs = Vec::with_capacity(self.list.len());
        let mut old = std::mem::take(&mut self.w.hs);
        for p in self.list.iter().filter(|p| p.gone == 0) {
            match old.binary_search_by_key(&p.pid, |h| h.0) {
                Ok(i) if old[i].2 == p.start => {
                    hs.push(old[i]);
                    old[i].3 = -1;
                }
                _ => {
                    let h = open(p.pid, PROCESS_QUERY_LIMITED_INFORMATION);
                    hs.push((p.pid, 0, p.start, h2fd(h), now));
                }
            }
        }
        for h in old {
            close(fd2h(h.3));
        }
        self.w.hs = hs;
        self.ok = true;
        // (procui spaces scans by their cost: a light one counts as 0.8 ms, so smooth
        // looks for new processes every 2 s, its reads between scans do the rest)
        self.cost = (crate::term::cpu_ns() - c0).max(if self.w.light > 0 { 800_000 } else { 0 });
    }

    /// The process `pid` of the basic list, known by its handle entry `i`?
    fn same(&self, i: usize, key: u64) -> bool {
        let h = self.w.hs[i];
        if h.1 != 0 {
            return h.1 == key;
        }
        // (seen in a full scan, whose list has no sequence numbers: the handle says
        // whether that process still runs; one that can't be opened is taken as it)
        let mut code = 0u32;
        h.3 < 0 || unsafe { GetExitCodeProcess(fd2h(h.3), &mut code) != 0 && code == 259 /* STILL_ACTIVE */ }
    }

    /// The basic list: processes that appeared (their name, user, times and memory
    /// through a new handle) and those that exited (`seen` not this generation).
    fn light_scan(&mut self, now: i64) {
        let mut list = std::mem::replace(&mut self.w.list, ProcList::new());
        if !list.read(false) {
            self.w.list = list;
            return;
        }
        self.gen = self.gen.wrapping_add(1);
        self.scans += 1;
        let gen = self.gen;
        let mut nk = 0;
        let mut ents: Vec<(u32, u32, u64, &[u16])> = Vec::new();
        list.each(&mut |e| {
            if e.pid != 0 {
                ents.push((e.pid, e.ppid, e.key, e.name));
            }
        });
        for &(pid, ppid, key, name) in &ents {
            let kt = pid == 4 || (ppid == 4 && !is_exe(name));
            if kt {
                nk += 1;
                if !self.show_k {
                    continue;
                }
            }
            if let Ok(i) = self.w.hs.binary_search_by_key(&pid, |h| h.0) {
                if self.same(i, key) {
                    self.w.hs[i].1 = key;
                    let start = self.w.hs[i].2;
                    if let Some(p) = bsearch(&self.list, pid).ok().and_then(|j| self.list.get_mut(j)).filter(|p| p.start == start) {
                        p.seen = gen;
                        p.ppid = ppid;
                    }
                    continue;
                }
            }
            // new (or the pid was reused): only what this user may open, until the
            // next full scan
            let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION);
            let Some((start, ticks, false)) = (!h.is_null()).then(|| times(h, self.w.boot)).flatten() else {
                close(h);
                continue;
            };
            let (txt, nlen, aoff, uid) = self.read_txt(pid, name, 1);
            let mut m: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
            unsafe { K32GetProcessMemoryInfo(h, &mut m, std::mem::size_of_val(&m) as u32) };
            let class = unsafe { GetPriorityClass(h) };
            let p = P {
                pid,
                ppid,
                uid,
                threads: 1,
                start,
                ticks,
                rss: m.WorkingSetSize as u64,
                io: [0; 2],
                cpu: 0.0,
                rd: 0.0,
                wr: 0.0,
                sv: 0.0,
                svt: 0,
                gone: 0,
                rank: u32::MAX,
                prank: u32::MAX,
                comm_h: 0,
                seen: gen,
                state: b'S',
                nice: nice_of(match class {
                    IDLE_PRIORITY_CLASS => 4,
                    BELOW_NORMAL_PRIORITY_CLASS => 6,
                    ABOVE_NORMAL_PRIORITY_CLASS => 10,
                    HIGH_PRIORITY_CLASS => 13,
                    REALTIME_PRIORITY_CLASS => 24,
                    _ => 8,
                }),
                flags: NORATE | NEW | if kt { KTHREAD } else { 0 },
                nlen,
                aoff,
                txt,
            };
            match bsearch(&self.list, pid) {
                Ok(j) => self.list[j] = p,
                Err(j) => self.list.insert(j, p),
            }
            match self.w.hs.binary_search_by_key(&pid, |h| h.0) {
                Ok(i) => {
                    close(fd2h(self.w.hs[i].3));
                    self.w.hs[i] = (pid, key, start, h2fd(h), now);
                }
                Err(i) => self.w.hs.insert(i, (pid, key, start, h2fd(h), now)),
            }
        }
        drop(ents);
        self.w.list = list;
        self.nk = nk;
    }

    /// A full scan: the system's process list, every process's times, memory, state,
    /// priority (and I/O with `io_all`).
    fn full_scan(&mut self, now: i64) {
        if !self.w.list.read(true) {
            return;
        }
        if self.me == u32::MAX {
            self.me = self.user_of(unsafe { GetCurrentProcess() }).unwrap_or(0);
        }
        self.gen = self.gen.wrapping_add(1);
        self.scans += 1;
        let gen = self.gen;
        let first = !self.ok;
        let dt = if self.ok {
            ((now - self.scan_t) as f32 / 1e9).max(0.05)
        } else {
            0.0
        };
        self.scan_t = now;
        let mut nk = 0;
        // (the records are read in place, the list taken out of `self` while new
        // processes' names and users are read)
        let mut ents: Vec<(u32, u32, &Spi, &[u16])> = Vec::new();
        let list = std::mem::replace(&mut self.w.list, ProcList::new());
        list.each(&mut |e| {
            if let (Some(s), true) = (e.full, e.pid != 0) {
                ents.push((e.pid, e.ppid, s, e.name));
            }
        });
        for &(pid, ppid, s, name) in &ents {
            // the kernel's own: System, Registry, Memory Compression, Secure System
            let kt = pid == 4 || (ppid == 4 && !is_exe(name));
            if kt {
                nk += 1;
                if !self.show_k {
                    continue;
                }
            }
            let start = (s.create - self.w.boot).max(0) as u64;
            let ticks = (s.kernel + s.user) as u64;
            let r = bsearch(&self.list, pid);
            let j = match r.ok().and_then(|j| self.list.get(j).map(|p| (j, p))) {
                Some((j, p)) if p.start == start && p.gone == 0 => j,
                _ => {
                    let (txt, nlen, aoff, uid) = self.read_txt(pid, name, s.session);
                    let p = P {
                        pid,
                        ppid,
                        uid,
                        threads: s.threads.min(65535) as u16,
                        start,
                        ticks,
                        rss: 0,
                        io: [0; 2],
                        cpu: 0.0,
                        rd: 0.0,
                        wr: 0.0,
                        sv: 0.0,
                        svt: 0,
                        gone: 0,
                        rank: u32::MAX,
                        prank: u32::MAX,
                        comm_h: 0,
                        seen: gen.wrapping_sub(1),
                        state: b'S',
                        nice: 0,
                        flags: NORATE | if kt { KTHREAD } else { 0 } | if first { 0 } else { NEW },
                        nlen,
                        aoff,
                        txt,
                    };
                    match r {
                        Ok(j) => {
                            self.list.remove(j);
                            self.list.insert(j, p);
                            j
                        }
                        Err(j) => {
                            self.list.insert(j, p);
                            j
                        }
                    }
                }
            };
            let io_all = self.io_all;
            let Some(p) = self.list.get_mut(j) else {
                continue;
            };
            // (rates over the time since this process's times were last read: a full
            // scan, or a read between scans)
            let last = self.w.hs.binary_search_by_key(&pid, |h| h.0).ok().filter(|&i| self.w.hs[i].2 == start);
            let t0 = last.map_or(0, |i| self.w.hs[i].4);
            if p.seen == gen.wrapping_sub(1) && t0 > 0 && now > t0 {
                p.cpu = ticks.saturating_sub(p.ticks) as f32 / ((now - t0) as f32 / 1e9 * HZ);
                p.flags &= !NORATE;
            } else {
                p.flags |= NORATE;
            }
            if let Some(i) = last {
                self.w.hs[i].4 = now;
            }
            p.seen = gen;
            p.ticks = ticks;
            p.ppid = ppid;
            p.state = state_of(s);
            p.nice = nice_of(s.prio);
            p.threads = s.threads.min(65535) as u16;
            p.rss = s.ws as u64;
            if io_all {
                // (all of the process's reads and writes: Windows counts disk, network
                // and pipes together)
                let (r, w) = (s.io[3] as u64, s.io[4] as u64);
                if p.flags & IOK != 0 && dt > 0.0 {
                    p.rd = (r as u32).wrapping_sub(p.io[0]) as f32 / dt;
                    p.wr = (w as u32).wrapping_sub(p.io[1]) as f32 / dt;
                }
                p.io = [r as u32, w as u32];
                p.flags |= IOK;
            }
        }
        drop(ents);
        self.w.list = list;
        self.nk = nk;
        self.denied = 0;
    }

    /// (Windows has no load average: the CPU page's estimate is used, see sys.rs)
    pub fn read_load(&mut self) {}

    /// Track exactly the processes in `want` (pid, start): open the new ones, close
    /// the others.
    pub fn track(&mut self, want: &[(u32, u64)]) {
        for t in self.tr.iter_mut() {
            t.want = false;
        }
        for &(pid, start) in want.iter().take(MAX_TRACK) {
            if let Some(t) = self.tr.iter_mut().find(|t| t.pid == pid && t.start == start) {
                t.want = true;
                continue;
            }
            let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION);
            if h.is_null() {
                continue;
            }
            if times(h, self.w.boot).map(|t| t.0) != Some(start) {
                close(h);
                continue;
            }
            self.tr.push(Track {
                pid,
                start,
                fd: h2fd(h),
                iofd: -1,
                mfd: -1,
                w: [(0, 0); WN],
                wi: 0,
                wn: 0,
                hc: [0.0; HN],
                hm: [0.0; HN],
                hlen: 0,
                hh: 0,
                want: true,
                io: (0, 0, 0),
            });
        }
        self.tr.retain_mut(|t| {
            if !t.want {
                t.close();
            }
            t.want
        });
    }

    /// Re-read the tracked processes (times and memory through their handles, I/O
    /// when `io`): CPU and I/O rates over about the last second. At most every 0.75 s:
    /// a process's times sum its threads' (~3–10 µs a process, and a grouped table
    /// tracks every member of the groups shown).
    pub fn read_tracked(&mut self, now: i64, io: bool) {
        if !io && now - self.w.tr_t < 750_000_000 {
            return;
        }
        self.w.tr_t = now;
        self.last_track = now;
        let ms = (now / 1_000_000) as u32 | 1;
        let boot = self.w.boot;
        let list = &mut self.list;
        for t in self.tr.iter_mut() {
            let h = fd2h(t.fd);
            let tm = times(h, boot).filter(|x| !x.2);
            let p = bsearch(list, t.pid)
                .ok()
                .and_then(|j| list.get_mut(j))
                .filter(|p| p.start == t.start);
            let (Some((_, ticks, _)), Some(p)) = (tm, p) else {
                if let Some(p) = bsearch(list, t.pid).ok().and_then(|j| list.get_mut(j)) {
                    if p.start == t.start && p.gone == 0 {
                        p.gone = ms;
                        p.cpu = 0.0;
                    }
                }
                t.close();
                continue;
            };
            let mut io_rate = None;
            if io && t.iofd != -2 {
                let mut c: windows_sys::Win32::System::Threading::IO_COUNTERS = unsafe { std::mem::zeroed() };
                if unsafe { GetProcessIoCounters(h, &mut c) } != 0 {
                    let (r, w) = (c.ReadTransferCount, c.WriteTransferCount);
                    let dt = (now - t.io.0) as f32 / 1e9;
                    if t.io.0 > 0 && dt > 0.2 {
                        io_rate = Some((r.saturating_sub(t.io.1) as f32 / dt, w.saturating_sub(t.io.2) as f32 / dt));
                    }
                    t.io = (now, r, w);
                    t.iofd = 0;
                } else {
                    t.iofd = -2;
                }
            }
            t.w[t.wi % WN] = (now, ticks);
            t.wi = (t.wi + 1) % WN;
            t.wn = (t.wn + 1).min(WN);
            let mut m: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
            if unsafe { K32GetProcessMemoryInfo(h, &mut m, std::mem::size_of_val(&m) as u32) } != 0 {
                p.rss = m.WorkingSetSize as u64;
            }
            if t.iofd == -2 {
                p.flags |= NOIO;
            }
            if let Some(c) = t.rates() {
                p.cpu = c / HZ;
                p.flags &= !NORATE;
            }
            if let Some((r, w)) = io_rate {
                p.rd = r;
                p.wr = w;
                p.flags |= IOK;
            }
        }
        self.tr.retain(|t| t.fd >= 0);
        // between scans, a few dozen of the other processes' times and memory (round
        // robin through the handles, at most once a second): the order and totals stay
        // current
        if now - self.w.rr_t < 950_000_000 {
            return;
        }
        self.w.rr_t = now;
        let n = self.w.hs.len();
        for _ in 0..n.min(CHUNK) {
            let i = self.w.rr % n;
            self.w.rr = self.w.rr.wrapping_add(1);
            let (pid, _, start, fd, t0) = self.w.hs[i];
            if fd < 0 || self.tr.iter().any(|t| t.pid == pid) {
                continue;
            }
            let Some((_, ticks, exited)) = times(fd2h(fd), boot) else {
                continue;
            };
            let Some(p) = bsearch(&self.list, pid).ok().and_then(|j| self.list.get_mut(j)).filter(|p| p.start == start) else {
                continue;
            };
            if exited {
                if p.gone == 0 {
                    p.gone = ms;
                    p.cpu = 0.0;
                }
                continue;
            }
            if t0 > 0 && now > t0 {
                p.cpu = ticks.saturating_sub(p.ticks) as f32 / ((now - t0) as f32 / 1e9 * HZ);
                p.flags &= !NORATE;
            }
            p.ticks = ticks;
            let mut m: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
            if unsafe { K32GetProcessMemoryInfo(fd2h(fd), &mut m, std::mem::size_of_val(&m) as u32) } != 0 {
                p.rss = m.WorkingSetSize as u64;
            }
            self.w.hs[i].4 = now;
        }
    }

    /// The start of the process now holding `pid` (None if there is none, or it can't
    /// be opened).
    pub fn start_of(&mut self, pid: u32) -> Option<u64> {
        let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION);
        if h.is_null() {
            return None;
        }
        let t = times(h, self.w.boot).filter(|t| !t.2).map(|t| t.0);
        close(h);
        t
    }

    /// The detail strip's facts: command line, the program's path (in `cwd`: another
    /// process's working directory can't be read without reading its memory), its
    /// session (in `cgroup`).
    pub fn detail(&mut self, pid: u32, start: u64, now: i64) -> Detail {
        let mut d = Detail {
            pid,
            start,
            t: now,
            st: Status {
                swap: u64::MAX,
                uid: u32::MAX,
                nspid: 0,
            },
            ..Default::default()
        };
        let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION);
        if h.is_null() {
            d.cwd = "?".into();
            return d;
        }
        let c = cmdline(h, &mut self.w.cbuf);
        clean(c.as_bytes(), 4096, &mut d.cmd);
        mask_secrets(&mut d.cmd, 0);
        let mut b = [0u16; 520];
        let mut n = b.len() as u32;
        d.cwd = if unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, b.as_mut_ptr(), &mut n) } != 0 {
            String::from_utf16_lossy(&b[..n as usize])
        } else {
            "?".into()
        };
        let mut sess = 0u32;
        if unsafe { ProcessIdToSessionId(pid, &mut sess) } != 0 {
            d.cgroup = sess.to_string();
        }
        close(h);
        d
    }

    /// (nothing kept between scans but the list's buffer, reused)
    pub fn release(&mut self) {}

    /// Act on the process `pid` that started at `start` (never on a later one reusing
    /// the pid: the handle pins it while the start time is checked): `sig` is the
    /// action's number in `sig` (end, suspend, resume).
    pub fn signal(&mut self, pid: u32, start: u64, sig: c_int) -> Result<(), c_int> {
        let access = if sig == 0 { PROCESS_TERMINATE } else { PROCESS_SUSPEND_RESUME };
        let h = open(pid, access | PROCESS_QUERY_LIMITED_INFORMATION);
        if h.is_null() {
            return Err(err_of(unsafe { windows_sys::Win32::Foundation::GetLastError() }));
        }
        let r = if times(h, self.w.boot).filter(|t| !t.2).map(|t| t.0) != Some(start) {
            Err(ESRCH)
        } else {
            let ok = match sig {
                0 => unsafe { TerminateProcess(h, 1) != 0 },
                1 => unsafe { NtSuspendProcess(h) >= 0 },
                _ => unsafe { NtResumeProcess(h) >= 0 },
            };
            if ok {
                Ok(())
            } else {
                Err(err_of(unsafe { windows_sys::Win32::Foundation::GetLastError() }))
            }
        };
        close(h);
        r
    }

    /// Set the process's priority class from a nice value (idle … realtime).
    pub fn renice(&mut self, pid: u32, start: u64, nice: i32) -> Result<(), c_int> {
        let h = open(pid, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION);
        if h.is_null() {
            return Err(err_of(unsafe { windows_sys::Win32::Foundation::GetLastError() }));
        }
        let r = if times(h, self.w.boot).filter(|t| !t.2).map(|t| t.0) != Some(start) {
            Err(ESRCH)
        } else if unsafe { SetPriorityClass(h, class_of(nice)) } != 0 {
            Ok(())
        } else {
            Err(err_of(unsafe { windows_sys::Win32::Foundation::GetLastError() }))
        };
        close(h);
        r
    }
}

fn err_of(e: u32) -> c_int {
    if e == ERROR_ACCESS_DENIED {
        EACCES
    } else {
        ESRCH
    }
}

/// Actions offered for `k`: Windows can end a process (TerminateProcess: no clean-up,
/// there is no polite request for programs in general), suspend and resume it.
pub const NSIG: usize = 3;
pub fn sig(i: usize) -> (&'static str, c_int, &'static str) {
    match i {
        1 => ("SUSPEND", 1, "pauses it (RESUME continues)"),
        2 => ("RESUME", 2, "continues a suspended process"),
        _ => ("END", 0, "ends it at once (no clean-up)"),
    }
}

/// Why an action failed, in words.
pub fn why(e: c_int) -> &'static str {
    match e {
        EPERM | EACCES => "permission denied (another user's process: run cpuview as administrator)",
        ESRCH => "it has already exited",
        _ => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scan of this machine's process list finds us, with sane numbers.
    #[test]
    fn scan_finds_this_process() {
        let mut p = Procs::new("");
        p.scan(crate::term::mono(), 0);
        // burn a little CPU, scan again: our own CPU time shows
        let mut x = 0u64;
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_millis() < 120 {
            x = x.wrapping_mul(31).wrapping_add(7);
        }
        std::hint::black_box(x);
        // (a full scan again: rates from the full list)
        p.w.light = FULL_EVERY;
        p.scan(crate::term::mono(), 0);
        let me = std::process::id();
        let j = p.find(me).expect("own process listed");
        let q = &p.list[j];
        assert!(q.rss > 0 && q.threads >= 1 && q.cpu > 0.2, "{} {}", q.rss, q.cpu);
        // a light scan, then the round-robin reads between scans: the rate again
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_millis() < 120 {
            x = x.wrapping_mul(31).wrapping_add(7);
        }
        std::hint::black_box(x);
        p.scan(crate::term::mono(), 0);
        assert!(p.w.light == 1 && p.find(me).is_some());
        for _ in 0..p.w.hs.len() / CHUNK + 2 {
            (p.w.rr_t, p.w.tr_t) = (0, 0);
            p.read_tracked(crate::term::mono(), false);
        }
        let q = &p.list[p.find(me).unwrap()];
        assert!(q.cpu > 0.2, "{}", q.cpu);
        assert_eq!(q.uid, p.me);
        assert!(p.user(q.uid) != "?");
        let start = q.start;
        assert!(p.list.windows(2).all(|w| w[0].pid < w[1].pid));
        assert_eq!(p.start_of(me), Some(start));
        p.track(&[(me, start)]);
        p.read_tracked(crate::term::mono(), true);
        assert_eq!(p.tr.len(), 1);
        let d = p.detail(me, start, 0);
        assert!(!d.cmd.is_empty() && d.cwd.ends_with(".exe"), "{} {}", d.cmd, d.cwd);
        // a wrong start time is never acted on (0 would end the process)
        assert_eq!(p.signal(me, start + 1, 0), Err(ESRCH));
        assert_eq!(p.renice(me, start + 1, 0), Err(ESRCH));
    }

    #[test]
    fn names_and_priorities() {
        assert_eq!(args_at(r#""C:\Program Files\x.exe" --a b"#), 25);
        assert_eq!(args_at("cmd /c dir"), 4);
        assert_eq!(args_at("tool"), 4);
        for n in [-20, -10, -5, 0, 10, 19] {
            let c = class_of(n);
            let base = match c {
                IDLE_PRIORITY_CLASS => 4,
                BELOW_NORMAL_PRIORITY_CLASS => 6,
                NORMAL_PRIORITY_CLASS => 8,
                ABOVE_NORMAL_PRIORITY_CLASS => 10,
                HIGH_PRIORITY_CLASS => 13,
                _ => 24,
            };
            assert_eq!(nice_of(base) as i32, n);
        }
    }
}

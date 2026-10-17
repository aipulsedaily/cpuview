//! Processes (view 4): every process in /proc, read cheaply and robustly.
//!
//! Costs (Ryzen 9950X, ~500 tasks): listing /proc 0.3 µs an entry; open + read + close
//! of a process's `stat` 1.6 µs (2.7 µs with cold caches); a pread of one kept open
//! 0.7 µs. So a *full scan* (list /proc, read every process's `stat`) runs only every
//! second or so (smooth; spaced further by its measured cost on big machines) or every
//! minute (eco), while the processes on screen and the selected one are *tracked*:
//! their `stat` and `io` stay open and are re-read with pread on every sample, which
//! gives their CPU % over the last second. Kernel threads are recognised once and then
//! skipped while hidden.
//!
//! Any /proc works: hidepid (only what is listed is shown, and why), unreadable files
//! (the fields show "?"), zombies, pid reuse (a process is its pid *and* start time),
//! thousands of processes (~150 bytes each), containers (the cgroup path names them).

use crate::sys::{parse_u64, pread};
use libc::c_int;
use std::rc::Rc;

/// Command line bytes kept per process for the table (the selected one is read whole).
const CMD_MAX: usize = 160;
/// PF_KTHREAD in the `flags` field of /proc/<pid>/stat
const PF_KTHREAD: u64 = 0x0020_0000;
/// CPU / disk rates use the samples of about the last second
const WIN_NS: i64 = 1_050_000_000;
const WN: usize = 12;
/// seconds of history per tracked process (the detail charts)
pub const HN: usize = 60;
/// tracked processes at most (two descriptors each)
const MAX_TRACK: usize = 240;

/// `P::flags`
pub const KTHREAD: u8 = 1;
/// /proc/<pid>/io not readable (another user's process)
pub const NOIO: u8 = 2;
/// seen in one scan only: no CPU rate yet
pub const NORATE: u8 = 4;
/// appeared after the first scan (fades in)
pub const NEW: u8 = 8;
/// disk rates known
pub const IOK: u8 = 16;

/// The fields of /proc/<pid>/stat this view uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stat {
    pub comm: [u8; 64],
    pub clen: usize,
    pub state: u8,
    pub ppid: u32,
    pub flags: u64,
    /// utime + stime (clock ticks)
    pub ticks: u64,
    pub nice: i8,
    pub threads: u32,
    /// start time, clock ticks after boot
    pub start: u64,
    /// resident pages
    pub rss: u64,
}

impl Stat {
    pub fn comm(&self) -> &[u8] {
        &self.comm[..self.clen]
    }
}

/// Parse /proc/<pid>/stat. The name (comm) may hold spaces and parentheses, so the
/// fields start after the *last* ')'.
pub fn parse_stat(b: &[u8]) -> Option<Stat> {
    let open = b.iter().position(|&c| c == b'(')?;
    let close = b.iter().rposition(|&c| c == b')')?;
    if close < open {
        return None;
    }
    let name = &b[open + 1..close];
    let mut s = Stat {
        comm: [0; 64],
        clen: name.len().min(64),
        state: b'?',
        ppid: 0,
        flags: 0,
        ticks: 0,
        nice: 0,
        threads: 1,
        start: 0,
        rss: 0,
    };
    s.comm[..s.clen].copy_from_slice(&name[..s.clen]);
    let mut n = 0;
    for f in b[close + 1..]
        .split(|&c| c == b' ' || c == b'\n')
        .filter(|f| !f.is_empty())
    {
        let v = || parse_u64(f).0;
        match n {
            0 => s.state = f[0],
            1 => s.ppid = v() as u32,
            6 => s.flags = v(),
            11 | 12 => s.ticks += v(),
            16 => {
                let neg = f[0] == b'-';
                let a = parse_u64(&f[neg as usize..]).0.min(128) as i16;
                s.nice = (if neg { -a } else { a }) as i8;
            }
            17 => s.threads = v() as u32,
            19 => s.start = v(),
            21 => {
                s.rss = v();
                return Some(s);
            }
            _ => {}
        }
        n += 1;
    }
    (n > 19).then_some(s)
}

/// /proc/<pid>/statm → (size, resident) in pages.
pub fn parse_statm(b: &[u8]) -> (u64, u64) {
    let (a, k) = parse_u64(b);
    (a, parse_u64(&b[k.min(b.len())..]).0)
}

/// `name: value` lines (/proc/<pid>/io, status): the value of `key`.
pub fn field(b: &[u8], key: &[u8]) -> Option<u64> {
    for l in b.split(|&c| c == b'\n') {
        if l.len() > key.len() && l.starts_with(key) && l[key.len()] == b':' {
            let r = &l[key.len() + 1..];
            let r = &r[r
                .iter()
                .position(|c| !c.is_ascii_whitespace())
                .unwrap_or(r.len())..];
            return Some(parse_u64(r).0);
        }
    }
    None
}

/// /proc/<pid>/io → (read_bytes, write_bytes): what actually reached the disks.
pub fn parse_io(b: &[u8]) -> Option<(u64, u64)> {
    Some((field(b, b"read_bytes")?, field(b, b"write_bytes")?))
}

/// What the detail strip shows from /proc/<pid>/status.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Status {
    /// real uid
    pub uid: u32,
    /// kB (u64::MAX: not reported, e.g. kernel threads)
    pub swap: u64,
    /// the pid inside the process's own pid namespace (containers), 0 if the same
    pub nspid: u32,
}

pub fn parse_status(b: &[u8]) -> Status {
    let mut s = Status {
        swap: field(b, b"VmSwap").unwrap_or(u64::MAX),
        uid: field(b, b"Uid").unwrap_or(u32::MAX as u64) as u32,
        nspid: 0,
    };
    // NSpid: one pid per namespace level, the process's own last
    if let Some(l) = b.split(|&c| c == b'\n').find(|l| l.starts_with(b"NSpid:")) {
        let mut v = l.rsplit(|&c| c == b'\t' || c == b' ');
        if let (Some(last), true) = (v.next(), l.iter().filter(|&&c| c == b'\t').count() > 1) {
            s.nspid = parse_u64(last).0 as u32;
        }
    }
    s
}

/// The program's name from its command line (NUL-separated arguments) and comm:
/// argv[0]'s file name (comm is often a thread's name, such as node's "MainThread"),
/// but a script's name when the interpreter runs one (comm names the script then),
/// and the first word when a process rewrote its title ("postgres: checkpointer").
/// Returns the name and the byte offset of the arguments after argv[0] when argv[0]
/// is the name (0 otherwise: the whole command line is worth showing).
pub fn prog_name<'a>(c: &'a [u8], comm: &'a [u8]) -> (&'a [u8], usize) {
    let c = &c[..c.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1)];
    if c.is_empty() {
        return (comm, 0);
    }
    let end = c.iter().position(|&b| b == 0).unwrap_or(c.len());
    let mut a0 = &c[..end];
    let mut args = end + 1;
    let base = |a: &'a [u8]| {
        let b = a.rsplit(|&x| x == b'/').next().unwrap_or(a);
        let k = b.iter().rposition(|&x| x != b':').map_or(0, |i| i + 1);
        &b[..k]
    };
    if end == c.len() {
        // one argument: a rewritten title ("nginx: worker process"); its program path
        // may hold spaces ("/opt/My Apps/chrome --type=renderer"): the first space
        // after which the file name is comm's, else the first
        let sps = || (0..a0.len()).filter(|&i| a0[i] == b' ');
        let sp = sps()
            .find(|&i| comm.len() >= 3 && base(&a0[..i]).starts_with(comm))
            .or_else(|| sps().next());
        if let Some(sp) = sp {
            a0 = &a0[..sp];
            args = sp + 1;
        }
    }
    let name = base(a0);
    // a script run by an interpreter: comm is the script's (first 15 bytes of its) name
    if comm.len() >= 3 && !name.starts_with(comm) {
        for a in c[end.min(c.len())..].split(|&b| b == 0).skip(1).take(2) {
            let b = base(a);
            if b.starts_with(comm) && b.len() <= 64 {
                return (b, 0);
            }
        }
    }
    if name.is_empty() {
        return (comm, 0);
    }
    (name, args.min(c.len()))
}

/// Printable text: invalid UTF-8 and control characters (NULs between arguments)
/// become spaces / U+FFFD, at most `max` bytes (cut at a character boundary).
fn clean(b: &[u8], max: usize, out: &mut String) {
    let s = String::from_utf8_lossy(b);
    for ch in s.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if out.len() + ch.len_utf8() > max {
            break;
        }
        out.push(ch);
    }
    // (the NUL after the last argument)
    while out.ends_with(' ') {
        out.pop();
    }
}

/// Hide secrets in a command line (from `at` on): the value after a flag like
/// `--authtoken`, `--token`, `--password`, `--secret`, `--api-key` (or in `--flag=value`),
/// of `NAME_TOKEN=` / `_KEY=` / `PASSWORD=` / `SECRET=` assignments, and the password in
/// `scheme://user:password@host` — shown as `••••`. A screen (or a screenshot of it) must
/// not hand out credentials.
pub fn mask_secrets(s: &mut String, at: usize) {
    const DOTS: &str = "••••";
    let secret = |k: &str| {
        let k = k.trim_start_matches('-').to_ascii_lowercase().replace('_', "-");
        k.ends_with("token")
            || k.ends_with("password")
            || k.ends_with("passwd")
            || k.ends_with("secret")
            || k.ends_with("api-key")
            || k.ends_with("apikey")
            || k.ends_with("access-key")
            || k.ends_with("private-key")
            || k == "pass"
            || k.ends_with("-key") && !k.starts_with("key")
    };
    let Some(tail) = s.get(at..) else { return };
    if !tail.contains(['=', ':', '-']) {
        return;
    }
    let mut out = String::with_capacity(tail.len());
    let mut hide_next = false;
    for (i, w) in tail.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if hide_next && !w.is_empty() {
            out.push_str(DOTS);
            hide_next = false;
            continue;
        }
        if let Some((k, v)) = w.split_once('=').filter(|(k, v)| !v.is_empty() && secret(k)) {
            let _ = v;
            out.push_str(k);
            out.push('=');
            out.push_str(DOTS);
        } else if let (Some(p), Some(a)) = (w.find("://"), w.find('@')) {
            // scheme://user:password@host
            match w[p + 3..a.max(p + 3)].find(':') {
                Some(c) if a > p + 3 => {
                    out.push_str(&w[..p + 3 + c + 1]);
                    out.push_str(DOTS);
                    out.push_str(&w[a..]);
                }
                _ => out.push_str(w),
            }
        } else {
            out.push_str(w);
            hide_next = w.starts_with('-') && secret(w);
        }
    }
    s.truncate(at);
    s.push_str(&out);
}

/// The cgroup path of a process (/proc/<pid>/cgroup): the v2 entry, else the v1
/// systemd (or first) hierarchy.
pub fn parse_cgroup(b: &[u8]) -> String {
    let t = String::from_utf8_lossy(b);
    let mut best = "";
    for l in t.lines() {
        // "id:controllers:path"
        let Some((head, _)) = l.split_once(":/") else {
            continue;
        };
        let path = l.get(head.len() + 1..).unwrap_or("");
        if head == "0:" {
            return path.to_string();
        }
        if best.is_empty() || head.ends_with(":name=systemd") {
            best = path;
        }
    }
    best.to_string()
}

/// One process. `txt` holds its name, a NUL, then its command line (the first
/// CMD_MAX bytes; empty for kernel threads and zombies).
pub struct P {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub threads: u16,
    pub start: u64,
    /// utime + stime at the last full scan
    pub ticks: u64,
    /// resident bytes
    pub rss: u64,
    /// read_bytes / write_bytes (low 32 bits) at the last full scan that read them
    pub io: [u32; 2],
    /// CPU time per second (1.0 = one thread busy), disk read / write bytes per second
    pub cpu: f32,
    pub rd: f32,
    pub wr: f32,
    /// the sort value (the live one with hysteresis) and since when (ms) they differ
    pub sv: f32,
    pub svt: u32,
    /// ms (monotonic) when it disappeared, 0 while alive
    pub gone: u32,
    /// row index at this and the previous layout (slides start there)
    pub rank: u32,
    pub prank: u32,
    comm_h: u16,
    seen: u16,
    pub state: u8,
    pub nice: i8,
    pub flags: u8,
    nlen: u8,
    aoff: u8,
    txt: Rc<str>,
}

impl P {
    pub fn name(&self) -> &str {
        &self.txt[..self.nlen as usize]
    }
    /// The whole command line as kept (empty for kernel threads and zombies).
    pub fn cmd(&self) -> &str {
        self.txt.get(self.nlen as usize + 1..).unwrap_or("")
    }
    /// What the table shows after the name: the arguments when argv[0] is the name,
    /// else the whole command line.
    pub fn args(&self) -> &str {
        self.cmd().get(self.aoff as usize..).unwrap_or("")
    }
    pub fn kthread(&self) -> bool {
        self.flags & KTHREAD != 0
    }
    /// Identity across pid reuse.
    pub fn key(&self) -> u64 {
        ((self.pid as u64) << 32) ^ self.start
    }
    /// The program a "group by program" row collects: the name, kernel threads by
    /// their name before the '/' (kworker/3:1 → kworker).
    pub fn group(&self) -> &str {
        let n = self.name();
        let k = if self.kthread() {
            n.bytes().position(|c| c == b'/' || c == b':')
        } else {
            None
        };
        n.get(..k.unwrap_or(n.len())).unwrap_or(n)
    }
}

/// The process `pid` in a list sorted by pid (else where it would go).
fn bsearch(list: &[P], pid: u32) -> Result<usize, usize> {
    list.binary_search_by_key(&pid, |p| p.pid)
}

fn hash(b: &[u8]) -> u32 {
    b.iter().fold(0x811c_9dc5u32, |h, &c| {
        (h ^ c as u32).wrapping_mul(0x0100_0193)
    })
}

/// A tracked process: `stat` and `io` kept open, samples of the last second, and the
/// detail charts' history.
pub struct Track {
    pub pid: u32,
    pub start: u64,
    fd: c_int,
    /// -1 not opened yet, -2 not readable
    iofd: c_int,
    /// statm: the exact resident size (stat's is the kernel's per-CPU estimate)
    mfd: c_int,
    /// (time, ticks), a ring
    w: [(i64, u64); WN],
    wi: usize,
    wn: usize,
    /// one point a second: CPU (% of one thread), resident bytes
    pub hc: [f32; HN],
    pub hm: [f32; HN],
    pub hlen: usize,
    hh: usize,
    want: bool,
    /// the last io read: (time, read_bytes, write_bytes)
    io: (i64, u64, u64),
}

impl Track {
    /// k = 0 the newest point.
    pub fn back(&self, h: &[f32; HN], k: usize) -> f32 {
        h[(self.hh + HN - 1 - k) % HN]
    }
    fn close(&mut self) {
        for fd in [self.fd, self.iofd, self.mfd] {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
        self.fd = -1;
        self.iofd = -1;
    }
    /// CPU ticks a second over about the last second; None until two samples at
    /// least 0.45 s apart.
    fn rates(&self) -> Option<f32> {
        if self.wn < 2 {
            return None;
        }
        let at = |k: usize| self.w[(self.wi + WN - 1 - k) % WN];
        let new = at(0);
        let mut r = at(1);
        for k in 2..self.wn {
            let e = at(k);
            if new.0 - e.0 > WIN_NS {
                break;
            }
            r = e;
        }
        let dt = (new.0 - r.0) as f32 / 1e9;
        if dt < 0.45 {
            return None;
        }
        Some(new.1.saturating_sub(r.1) as f32 / dt)
    }
}

/// What the detail strip shows about the selected process (read when it is
/// selected, then every few seconds).
#[derive(Default)]
pub struct Detail {
    pub pid: u32,
    pub start: u64,
    pub cmd: String,
    pub cwd: String,
    pub cgroup: String,
    pub st: Status,
    pub t: i64,
}

/// /proc's hidepid mount option: 0 none, 1 noaccess, 2 invisible, 4 ptraceable.
pub fn hidepid(mountinfo: &str, root: &str) -> u8 {
    // "<id> <parent> <dev> <root> <mount point> <options> ... - proc proc <options>"
    for l in mountinfo.lines() {
        if l.split(' ').nth(4) != Some(root) {
            continue;
        }
        let Some((_, sup)) = l.split_once(" - proc ") else {
            continue;
        };
        if let Some((_, v)) = sup.split_once("hidepid=") {
            return match v.split(',').next().unwrap_or("").trim() {
                "1" | "noaccess" => 1,
                "2" | "invisible" => 2,
                "4" | "ptraceable" => 4,
                _ => 0,
            };
        }
    }
    0
}

pub struct Procs {
    /// every process, by pid
    pub list: Vec<P>,
    pub tr: Vec<Track>,
    dir: *mut libc::DIR,
    dfd: c_int,
    load_fd: c_int,
    pids: Vec<u32>,
    /// kernel threads' pids (skipped while hidden); rebuilt every 60th scan
    skip: Vec<u32>,
    buf: Vec<u8>,
    users: Vec<(u32, Box<str>)>,
    gen: u16,
    pub scans: u32,
    /// clock ticks a second
    pub hz: f32,
    page: u64,
    pub show_k: bool,
    /// read every process's io in full scans (sorting by disk)
    pub io_all: bool,
    /// kernel threads, processes listed but not readable
    pub nk: u32,
    pub denied: u32,
    /// /proc's hidepid mode (see `hidepid`), our uid
    pub hide: u8,
    pub me: u32,
    /// load average (1, 5, 15 minutes)
    pub load: [f32; 3],
    pub scan_t: i64,
    /// CPU ns the last full scan took
    pub cost: i64,
    pub last_track: i64,
    pub ok: bool,
    /// tracked processes' exact resident size (`statm`) and command lines; else
    /// stat's estimate, names only (the overview)
    pub statm: bool,
}

/// errno of the last failed call.
pub fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// "<pid>/<file>\0" for openat relative to /proc.
fn path(pid: u32, file: &str, out: &mut [u8; 48]) {
    let mut f = crate::ui::F::new();
    f.int(pid, 0);
    f.push(b'/');
    f.str(file);
    let n = f.n.min(47);
    out[..n].copy_from_slice(&f.s().as_bytes()[..n]);
    out[n] = 0;
}

impl Procs {
    pub fn new(root: &str) -> Procs {
        let c = std::ffi::CString::new(root).unwrap_or_default();
        let dir = unsafe { libc::opendir(c.as_ptr()) };
        // (its own descriptor: the listing can be closed between scans, `release`)
        let dfd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let mut p = Procs {
            list: Vec::new(),
            tr: Vec::new(),
            dir,
            dfd,
            load_fd: -1,
            pids: Vec::new(),
            skip: Vec::new(),
            buf: vec![0; 4096],
            users: Vec::new(),
            gen: 0,
            scans: 0,
            hz: (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f32,
            page: unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1024) as u64,
            show_k: false,
            io_all: false,
            nk: 0,
            denied: 0,
            hide: hidepid(&mi, root.trim_end_matches('/')),
            me: unsafe { libc::geteuid() },
            load: [0.0; 3],
            scan_t: 0,
            cost: 0,
            last_track: i64::MIN / 2,
            ok: false,
            statm: true,
        };
        p.load_fd = p.open(&format!("{root}/loadavg"));
        p
    }

    fn open(&self, path: &str) -> c_int {
        crate::sys::open_ro(path)
    }

    fn openat(&self, pid: u32, file: &str) -> c_int {
        if self.dfd < 0 {
            return -1;
        }
        let mut p = [0u8; 48];
        path(pid, file, &mut p);
        unsafe {
            libc::openat(
                self.dfd,
                p.as_ptr() as *const _,
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        }
    }

    /// Read a whole small file of process `pid` into `buf` (up to its size); bytes read
    /// or None (errno tells why).
    fn read_file(&mut self, pid: u32, file: &str) -> Option<&[u8]> {
        let fd = self.openat(pid, file);
        if fd < 0 {
            return None;
        }
        let n = unsafe { libc::read(fd, self.buf.as_mut_ptr() as *mut _, self.buf.len()) };
        let e = errno();
        unsafe { libc::close(fd) };
        if n < 0 {
            unsafe { *libc::__errno_location() = e };
            return None;
        }
        self.buf.get(..n as usize)
    }

    pub fn find(&self, pid: u32) -> Option<usize> {
        bsearch(&self.list, pid).ok()
    }

    /// The process `pid` started at `start` (not a later one reusing the pid).
    pub fn get(&self, pid: u32, start: u64) -> Option<&P> {
        self.list.get(self.find(pid)?).filter(|p| p.start == start)
    }

    /// The user name of `uid` (cached: /etc/passwd, then the system's user database,
    /// else the number).
    pub fn user(&self, uid: u32) -> &str {
        self.users.iter().find(|u| u.0 == uid).map_or("?", |u| &u.1)
    }

    fn learn_user(&mut self, uid: u32) {
        if self.users.iter().any(|u| u.0 == uid) {
            return;
        }
        let pw = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
        let mut name = pw.lines().find_map(|l| {
            let mut f = l.split(':');
            let n = f.next()?;
            (f.nth(1)?.parse::<u32>().ok()? == uid).then(|| n.to_string())
        });
        // LDAP / sssd users (login uids; the user database loads its modules, ~0.4 MB,
        // so not for containers' system uids or systemd's dynamic ones: numbers)
        if name.is_none() && (1000..60000).contains(&uid) {
            let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut b = [0 as libc::c_char; 1024];
            let mut res = std::ptr::null_mut();
            unsafe { libc::getpwuid_r(uid, &mut pwd, b.as_mut_ptr(), b.len(), &mut res) };
            if !res.is_null() && !pwd.pw_name.is_null() {
                name = Some(
                    unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
                        .to_string_lossy()
                        .to_string(),
                );
            }
        }
        let name = name.unwrap_or_else(|| uid.to_string());
        self.users.push((uid, name.into()));
    }

    /// Name and command line of a process seen for the first time (or after an exec):
    /// (text, name length, arguments offset, comm hash).
    fn read_txt(&mut self, pid: u32, st: &Stat) -> (Rc<str>, u8, u8, u16) {
        let whole = self.statm;
        let c = self.read_file(pid, "cmdline").unwrap_or(&[]);
        let (name, aoff) = prog_name(c, st.comm());
        let mut s = String::new();
        clean(name, 64, &mut s);
        let nlen = s.len() as u8;
        s.push('\0');
        if whole {
            // (the overview, `statm` off, shows names only)
            let at = s.len();
            clean(c, CMD_MAX + s.len(), &mut s);
            mask_secrets(&mut s, at);
        }
        // (the offset counts bytes of the raw command line: right while it's ASCII)
        let aoff = if c.get(..aoff).is_some_and(|a| a.is_ascii()) {
            aoff.min(255) as u8
        } else {
            0
        };
        // shared with a neighbour (by pid) running the same command: worker pools
        // started together keep one copy
        let j = bsearch(&self.list, pid).unwrap_or_else(|j| j);
        let txt = (j.saturating_sub(1)..j + 2)
            .filter_map(|k| self.list.get(k))
            .find(|q| *q.txt == *s)
            .map_or_else(|| Rc::from(s), |q| q.txt.clone());
        (txt, nlen, aoff, hash(st.comm()) as u16)
    }

    /// A full scan: list /proc, read every process's `stat` (and `io` with `io_all`),
    /// note new and exited processes. `keep_ms`: exited ones stay that long (fading).
    pub fn scan(&mut self, now: i64, keep_ms: u32) {
        if self.dir.is_null() && self.dfd >= 0 {
            // (released: list the directory again)
            self.dir = unsafe { libc::fdopendir(libc::fcntl(self.dfd, libc::F_DUPFD_CLOEXEC, 0)) };
        }
        if self.dir.is_null() {
            return;
        }
        let c0 = crate::term::cpu_ns();
        self.gen = self.gen.wrapping_add(1);
        self.scans += 1;
        let gen = self.gen;
        let first = !self.ok;
        let fresh_skip = self.scans % 60 == 1 || self.show_k;
        if fresh_skip {
            self.skip.clear();
        }
        // list (/proc lists pids in increasing order, which the skip list relies on;
        // the process list is kept in pid order whatever the order here)
        self.pids.clear();
        unsafe { libc::rewinddir(self.dir) };
        loop {
            let e = unsafe { libc::readdir(self.dir) };
            if e.is_null() {
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*e).d_name.as_ptr()) }.to_bytes();
            if name.is_empty() || name.len() > 10 || !name.iter().all(u8::is_ascii_digit) {
                continue;
            }
            self.pids.push(parse_u64(name).0 as u32);
        }
        let dt = if self.ok {
            ((now - self.scan_t) as f32 / 1e9).max(0.05)
        } else {
            0.0
        };
        self.scan_t = now;
        let (mut nk, mut denied) = (0, 0);
        let mut sk = 0;
        let pids = std::mem::take(&mut self.pids);
        for &pid in &pids {
            if !fresh_skip {
                while sk < self.skip.len() && self.skip[sk] < pid {
                    sk += 1;
                }
                if self.skip.get(sk) == Some(&pid) {
                    nk += 1;
                    continue;
                }
            }
            let Some(b) = self.read_file(pid, "stat") else {
                if matches!(errno(), libc::EACCES | libc::EPERM) {
                    denied += 1;
                }
                continue;
            };
            let Some(st) = parse_stat(b) else {
                continue;
            };
            let kt = st.flags & PF_KTHREAD != 0 || (st.ppid == 2 && st.rss == 0) || pid == 2;
            if kt {
                nk += 1;
                if !self.show_k {
                    if fresh_skip {
                        self.skip.push(pid);
                    }
                    continue;
                }
            }
            let r = bsearch(&self.list, pid);
            let j = match r.ok().and_then(|j| self.list.get(j).map(|p| (j, p))) {
                Some((j, p)) if p.start == st.start && p.gone == 0 => j,
                _ => {
                    // new (or the pid was reused): a fresh entry
                    let (txt, nlen, aoff, comm_h) = self.read_txt(pid, &st);
                    let mut p = P {
                        pid,
                        ppid: st.ppid,
                        uid: u32::MAX,
                        threads: st.threads.min(65535) as u16,
                        start: st.start,
                        ticks: st.ticks,
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
                        comm_h,
                        seen: gen.wrapping_sub(1),
                        state: st.state,
                        nice: st.nice,
                        flags: NORATE | if kt { KTHREAD } else { 0 } | if first { 0 } else { NEW },
                        nlen,
                        aoff,
                        txt,
                    };
                    // owner: the /proc/<pid> directory's (the effective uid)
                    let mut sb: libc::stat = unsafe { std::mem::zeroed() };
                    let mut pp = [0u8; 48];
                    path(pid, "", &mut pp);
                    if unsafe { libc::fstatat(self.dfd, pp.as_ptr() as *const _, &mut sb, 0) } == 0
                    {
                        p.uid = sb.st_uid;
                        self.learn_user(p.uid);
                    }
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
            let page = self.page;
            let io_all = self.io_all;
            // exec'd (or renamed itself): name and command line again
            let exec = self
                .list
                .get(j)
                .is_some_and(|p| p.comm_h != hash(st.comm()) as u16);
            let txt = exec.then(|| self.read_txt(pid, &st));
            let Some(p) = self.list.get_mut(j) else {
                continue;
            };
            if let Some(t) = txt {
                (p.txt, p.nlen, p.aoff, p.comm_h) = t;
            }
            if p.seen == gen.wrapping_sub(1) && dt > 0.0 {
                p.cpu = st.ticks.saturating_sub(p.ticks) as f32 / (dt * self.hz);
                p.flags &= !NORATE;
            } else {
                p.flags |= NORATE;
            }
            p.seen = gen;
            p.ticks = st.ticks;
            p.ppid = st.ppid;
            p.state = st.state;
            p.nice = st.nice;
            p.threads = st.threads.min(65535) as u16;
            p.rss = st.rss * page;
            if io_all && p.flags & NOIO == 0 {
                let io = self.read_file(pid, "io").and_then(parse_io);
                let denied = io.is_none() && matches!(errno(), libc::EACCES | libc::EPERM);
                let Some(p) = self.list.get_mut(j) else {
                    continue;
                };
                if let Some((r, w)) = io {
                    if p.flags & IOK != 0 && dt > 0.0 {
                        p.rd = (r as u32).wrapping_sub(p.io[0]) as f32 / dt;
                        p.wr = (w as u32).wrapping_sub(p.io[1]) as f32 / dt;
                    }
                    p.io = [r as u32, w as u32];
                    p.flags |= IOK;
                }
                if denied {
                    p.flags |= NOIO;
                }
            }
        }
        self.pids = pids;
        self.nk = nk;
        self.denied = denied;
        // exited: kept `keep_ms` (rows fade out), then dropped
        let ms = (now / 1_000_000) as u32 | 1;
        for p in self.list.iter_mut() {
            if p.seen != gen && p.gone == 0 {
                p.gone = ms;
                p.cpu = 0.0;
            }
        }
        self.list
            .retain(|p| p.gone == 0 || (ms.wrapping_sub(p.gone) < keep_ms && keep_ms > 0));
        // tracked ones whose process is gone
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
        self.ok = true;
        self.read_load();
        self.cost = crate::term::cpu_ns() - c0;
    }

    /// The load average (/proc/loadavg).
    pub fn read_load(&mut self) {
        let mut b = [0u8; 128];
        let n = pread(self.load_fd, &mut b);
        let s = std::str::from_utf8(b.get(..n).unwrap_or(&[])).unwrap_or("");
        let mut it = s.split(' ');
        for l in self.load.iter_mut() {
            *l = it.next().and_then(crate::sys::pf).unwrap_or(0.0);
        }
    }

    /// Track exactly the processes in `want` (pid, start): open the new ones, close
    /// the others.
    pub fn track(&mut self, want: &[(u32, u64)]) {
        for t in self.tr.iter_mut() {
            t.want = false;
        }
        for &(pid, start) in want.iter().take(MAX_TRACK) {
            if let Some(t) = self
                .tr
                .iter_mut()
                .find(|t| t.pid == pid && t.start == start)
            {
                t.want = true;
                continue;
            }
            let fd = self.openat(pid, "stat");
            if fd < 0 {
                continue;
            }
            let mfd = if self.statm { self.openat(pid, "statm") } else { -1 };
            self.tr.push(Track {
                pid,
                start,
                fd,
                iofd: -1,
                mfd,
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

    /// Re-read the tracked processes (pread of their open `stat`, and `io` when `io`):
    /// CPU and disk rates over about the last second.
    pub fn read_tracked(&mut self, now: i64, io: bool) {
        self.last_track = now;
        let page = self.page;
        let ms = (now / 1_000_000) as u32 | 1;
        let hz = self.hz;
        let dfd = self.dfd;
        let mut b = [0u8; 1024];
        let list = &mut self.list;
        for t in self.tr.iter_mut() {
            let n = pread(t.fd, &mut b);
            let st = b.get(..n).and_then(parse_stat);
            let p = bsearch(list, t.pid)
                .ok()
                .and_then(|j| list.get_mut(j))
                .filter(|p| p.start == t.start);
            let (Some(st), Some(p)) = (st, p) else {
                // exited (reads of a dead process's files fail): fades out
                if let Some(p) = bsearch(list, t.pid).ok().and_then(|j| list.get_mut(j)) {
                    if p.start == t.start && p.gone == 0 {
                        p.gone = ms;
                        p.cpu = 0.0;
                    }
                }
                t.close();
                continue;
            };
            if io && t.iofd == -1 {
                let mut pa = [0u8; 48];
                path(t.pid, "io", &mut pa);
                let fd = unsafe {
                    libc::openat(
                        dfd,
                        pa.as_ptr() as *const _,
                        libc::O_RDONLY | libc::O_CLOEXEC,
                    )
                };
                t.iofd = if fd >= 0 { fd } else { -2 };
            }
            let mut rw = None;
            if io && t.iofd >= 0 {
                let n = pread(t.iofd, &mut b);
                rw = b.get(..n).and_then(parse_io);
                if rw.is_none() {
                    unsafe { libc::close(t.iofd) };
                    t.iofd = -2;
                }
            }
            // disk rates between io reads (about a second apart)
            let mut io_rate = None;
            if let Some((r, w)) = rw {
                let dt = (now - t.io.0) as f32 / 1e9;
                if t.io.0 > 0 && dt > 0.2 {
                    io_rate = Some((
                        r.saturating_sub(t.io.1) as f32 / dt,
                        w.saturating_sub(t.io.2) as f32 / dt,
                    ));
                }
                t.io = (now, r, w);
            }
            t.w[t.wi % WN] = (now, st.ticks);
            t.wi = (t.wi + 1) % WN;
            t.wn = (t.wn + 1).min(WN);
            let denied = t.iofd == -2;
            p.state = st.state;
            p.nice = st.nice;
            p.threads = st.threads.min(65535) as u16;
            let n = pread(t.mfd, &mut b);
            p.rss = b
                .get(..n)
                .map_or(0, |m| parse_statm(m).1)
                .max(st.rss * (n == 0) as u64)
                * page;
            p.ppid = st.ppid;
            if denied {
                p.flags |= NOIO;
            }
            if let Some(c) = t.rates() {
                p.cpu = c / hz;
                p.flags &= !NORATE;
            }
            if let Some((r, w)) = io_rate {
                p.rd = r;
                p.wr = w;
                p.flags |= IOK;
            }
        }
        self.tr.retain(|t| t.fd >= 0);
    }

    /// One history point a second (`n` of them after a longer gap) for every tracked
    /// process.
    pub fn hist_push(&mut self, n: u32) {
        for t in self.tr.iter_mut() {
            let Some(p) = bsearch(&self.list, t.pid)
                .ok()
                .and_then(|j| self.list.get(j))
            else {
                continue;
            };
            for _ in 0..n.min(HN as u32) {
                let h = t.hh % HN;
                t.hc[h] = p.cpu * 100.0;
                t.hm[h] = p.rss as f32;
                t.hh = (h + 1) % HN;
                t.hlen = (t.hlen + 1).min(HN);
            }
        }
    }

    pub fn tracked(&self, pid: u32, start: u64) -> Option<&Track> {
        self.tr.iter().find(|t| t.pid == pid && t.start == start)
    }

    /// The start time of the process now holding `pid` (None if there is none).
    pub fn start_of(&mut self, pid: u32) -> Option<u64> {
        parse_stat(self.read_file(pid, "stat")?).map(|s| s.start)
    }

    /// The detail strip's facts about a process: command line (its first 4 KB: more
    /// than the strip shows), working directory, cgroup / container, status.
    pub fn detail(&mut self, pid: u32, start: u64, now: i64) -> Detail {
        let mut d = Detail {
            pid,
            start,
            t: now,
            ..Default::default()
        };
        if let Some(c) = self.read_file(pid, "cmdline") {
            clean(c, 4096, &mut d.cmd);
            mask_secrets(&mut d.cmd, 0);
        }
        let mut p = [0u8; 48];
        path(pid, "cwd", &mut p);
        let n = unsafe {
            libc::readlinkat(
                self.dfd,
                p.as_ptr() as *const _,
                self.buf.as_mut_ptr() as *mut _,
                self.buf.len(),
            )
        };
        d.cwd = match self.buf.get(..n.max(0) as usize) {
            Some(b) if n > 0 => String::from_utf8_lossy(b).to_string(),
            _ => "?".into(),
        };
        if let Some(b) = self.read_file(pid, "cgroup") {
            d.cgroup = parse_cgroup(b);
        }
        d.st = self.read_file(pid, "status").map_or(
            Status {
                swap: u64::MAX,
                uid: u32::MAX,
                ..Default::default()
            },
            parse_status,
        );
        d
    }

    /// Kernel threads: show them (`true`, read every scan) or skip them.
    pub fn set_show_k(&mut self, on: bool) {
        self.show_k = on;
        if !on {
            self.list.retain(|p| !p.kthread());
        }
        self.skip.clear();
        self.scans = 0;
    }

    /// Close the directory listing (its buffer is 32 KB) until the next scan.
    pub fn release(&mut self) {
        if !self.dir.is_null() {
            unsafe { libc::closedir(self.dir) };
            self.dir = std::ptr::null_mut();
        }
    }

    /// Forget everything (leaving the page): the next scan starts afresh.
    pub fn reset(&mut self) {
        for t in self.tr.iter_mut() {
            t.close();
        }
        self.tr = Vec::new();
        self.list = Vec::new();
        self.skip = Vec::new();
        self.pids = Vec::new();
        self.ok = false;
        self.scans = 0;
    }
}

// ── actions ────────────────────────────────────────────────────────────────

/// Signals offered for `k` (0, TERM, is the default): name, number, what it does.
pub const NSIG: usize = 6;
pub fn sig(i: usize) -> (&'static str, c_int, &'static str) {
    match i {
        1 => ("KILL", libc::SIGKILL, "ends it at once (no clean-up)"),
        2 => (
            "HUP",
            libc::SIGHUP,
            "hang-up: many daemons reload their settings",
        ),
        3 => ("INT", libc::SIGINT, "as Ctrl-C"),
        4 => ("STOP", libc::SIGSTOP, "pauses it (CONT resumes)"),
        5 => ("CONT", libc::SIGCONT, "resumes a stopped process"),
        _ => (
            "TERM",
            libc::SIGTERM,
            "asks it to exit (it can clean up first)",
        ),
    }
}

/// Why an action failed, in words.
pub fn why(e: c_int) -> &'static str {
    match e {
        libc::EPERM | libc::EACCES => {
            "permission denied (another user's process: run cpuview as root)"
        }
        libc::ESRCH => "it has already exited",
        _ => "failed",
    }
}

impl Procs {
    /// Send `sig` to the process `pid` that started at `start` (never to a later one
    /// reusing the pid: a pidfd pins it while the start time is checked).
    pub fn signal(&mut self, pid: u32, start: u64, sig: c_int) -> Result<(), c_int> {
        let pfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as c_int, 0) } as c_int;
        let r = if self.start_of(pid) != Some(start) {
            Err(libc::ESRCH)
        } else {
            let rc = if pfd >= 0 {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pfd,
                        sig,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                }
            } else {
                unsafe { libc::kill(pid as libc::pid_t, sig) as libc::c_long }
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(errno())
            }
        };
        if pfd >= 0 {
            unsafe { libc::close(pfd) };
        }
        r
    }

    /// Set the nice value of every thread of the process.
    pub fn renice(&mut self, pid: u32, start: u64, nice: i32) -> Result<(), c_int> {
        if self.start_of(pid) != Some(start) {
            return Err(libc::ESRCH);
        }
        // every thread (/proc/<pid>/task): setpriority acts on one thread
        let mut tids = Vec::new();
        let fd = self.openat(pid, "task");
        let dir = if fd >= 0 {
            unsafe { libc::fdopendir(fd) }
        } else {
            std::ptr::null_mut()
        };
        while !dir.is_null() {
            let e = unsafe { libc::readdir(dir) };
            if e.is_null() {
                unsafe { libc::closedir(dir) };
                break;
            }
            let n = unsafe { std::ffi::CStr::from_ptr((*e).d_name.as_ptr()) }.to_bytes();
            if n.first().is_some_and(u8::is_ascii_digit) {
                tids.push(parse_u64(n).0 as u32);
            }
        }
        if tids.is_empty() {
            tids.push(pid);
        }
        let mut err = 0;
        let mut ok = false;
        for t in tids {
            if unsafe { libc::setpriority(libc::PRIO_PROCESS, t as libc::id_t, nice) } == 0 {
                ok = true;
            } else if err == 0 {
                err = errno();
            }
        }
        if ok {
            Ok(())
        } else {
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_names_with_spaces_and_parentheses() {
        let b = b"1234 (my (weird) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 3 0 98765 123456789 512 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 5 0 0 0 0 0\n";
        let s = parse_stat(b).unwrap();
        assert_eq!(s.comm(), b"my (weird) name");
        assert_eq!(s.state, b'S');
        assert_eq!(s.ppid, 1);
        assert_eq!(s.ticks, 300);
        assert_eq!(s.nice, 0);
        assert_eq!(s.threads, 3);
        assert_eq!(s.start, 98765);
        assert_eq!(s.rss, 512);
        assert_eq!(s.flags & PF_KTHREAD, 0);
        // negative nice, a name that is just ")", kernel thread flag
        let b = b"2 ()) I 0 0 0 0 -1 2129984 0 0 0 0 0 7 0 0 0 -20 1 0 12 0 0 18446744073709551615";
        let s = parse_stat(b).unwrap();
        assert_eq!(s.comm(), b")");
        assert_eq!((s.state, s.nice, s.ticks, s.start), (b'I', -20, 7, 12));
        assert_ne!(s.flags & PF_KTHREAD, 0);
        // zombie, truncated or garbage input
        let b = b"77 (defunct thing) Z 76 77 77 0 -1 4227084 0 0 0 0 0 0 0 0 20 0 1 0 555 0 0 18446744073709551615";
        assert_eq!(parse_stat(b).unwrap().state, b'Z');
        assert!(parse_stat(b"12 (x) S 1 2").is_none());
        assert!(parse_stat(b"").is_none());
        assert!(parse_stat(b"garbage)( 1").is_none());
    }

    #[test]
    fn statm_io_status() {
        assert_eq!(parse_statm(b"5000 1234 300 10 0 900 0\n"), (5000, 1234));
        assert_eq!(parse_statm(b""), (0, 0));
        let io = b"rchar: 1000\nwchar: 2000\nsyscr: 5\nsyscw: 6\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_io(io), Some((4096, 8192)));
        assert_eq!(parse_io(b"rchar: 1\n"), None);
        let st = b"Name:\tpostgres\nUmask:\t0077\nState:\tS (sleeping)\nTgid:\t812\nNgid:\t0\nPid:\t812\nPPid:\t1\nUid:\t114\t114\t114\t114\nGid:\t121\t121\t121\t121\nNSpid:\t812\t7\nVmRSS:\t   29304 kB\nVmSwap:\t       12 kB\nThreads:\t1\nvoluntary_ctxt_switches:\t1520\nnonvoluntary_ctxt_switches:\t33\n";
        let s = parse_status(st);
        assert_eq!(
            s,
            Status {
                uid: 114,
                swap: 12,
                nspid: 7,
            }
        );
        // kernel thread: no Vm lines, one pid namespace
        let s = parse_status(b"Name:\tkworker/0:1\nUid:\t0\t0\t0\t0\nNSpid:\t42\n");
        assert_eq!((s.uid, s.swap, s.nspid), (0, u64::MAX, 0));
    }

    #[test]
    fn program_names() {
        let n = |c: &[u8], comm: &[u8]| {
            let (n, a) = prog_name(c, comm);
            (String::from_utf8_lossy(n).to_string(), a)
        };
        assert_eq!(
            n(b"/usr/bin/node\0server.js\0", b"MainThread"),
            ("node".into(), 14)
        );
        assert_eq!(
            n(b"postgres: checkpointer \0\0\0", b"postgres"),
            ("postgres".into(), 10)
        );
        assert_eq!(
            n(
                b"/usr/bin/python3\0-u\0/opt/tool/sync-files.py\0",
                b"sync-files.py"
            ),
            ("sync-files.py".into(), 0)
        );
        assert_eq!(n(b"", b"kworker/0:1"), ("kworker/0:1".into(), 0));
        assert_eq!(n(b"/opt/My App/app\0--x\0", b"app"), ("app".into(), 16));
        assert_eq!(n(b"-bash\0", b"bash"), ("-bash".into(), 5));
        // a rewritten title whose path holds spaces
        assert_eq!(
            n(
                b"/home/u/My Apps/chrome-headless-shell --type=renderer",
                b"chrome-headless"
            ),
            ("chrome-headless-shell".into(), 38)
        );
    }

    #[test]
    fn cgroups() {
        assert_eq!(
            parse_cgroup(b"12:pids:/user.slice\n1:name=systemd:/system.slice/x.service\n"),
            "/system.slice/x.service"
        );
        assert_eq!(parse_cgroup(b"0::/init.scope\n"), "/init.scope");
    }

    #[test]
    fn hidepid_option() {
        let mi = "22 28 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw,hidepid=invisible\n";
        assert_eq!(hidepid(mi, "/proc"), 2);
        let mi = "22 28 0:21 / /proc rw,relatime - proc proc rw,hidepid=1,gid=4\n";
        assert_eq!(hidepid(mi, "/proc"), 1);
        assert_eq!(
            hidepid("22 28 0:21 / /proc rw - proc proc rw\n", "/proc"),
            0
        );
    }

    /// A scan of this machine's /proc finds us, with sane numbers.
    #[test]
    fn scan_finds_this_process() {
        let mut p = Procs::new("/proc");
        let t = crate::term::mono();
        p.scan(t, 0);
        // burn a little CPU, scan again: our own CPU time shows
        let mut x = 0u64;
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_millis() < 120 {
            x = x.wrapping_mul(31).wrapping_add(7);
        }
        std::hint::black_box(x);
        p.scan(crate::term::mono(), 0);
        let me = std::process::id();
        let j = p.find(me).expect("own process listed");
        let q = &p.list[j];
        assert!(
            q.rss > 0 && q.threads >= 1 && q.cpu > 0.2,
            "{} {}",
            q.rss,
            q.cpu
        );
        assert_eq!(q.uid, unsafe { libc::geteuid() });
        let start = q.start;
        assert!(p.list.windows(2).all(|w| w[0].pid < w[1].pid));
        // tracked: open files re-read
        p.track(&[(me, start)]);
        p.read_tracked(crate::term::mono(), true);
        assert_eq!(p.tr.len(), 1);
        let d = p.detail(me, start, 0);
        assert!(!d.cmd.is_empty() && d.cwd.starts_with('/'));
        // a wrong start time is never signalled
        assert_eq!(p.signal(me, start + 1, 0), Err(libc::ESRCH));
        assert_eq!(p.signal(me, start, 0), Ok(()));
    }

    #[test]
    fn secrets_are_masked() {
        let mut c = String::from("ngrok http 80 --authtoken 2abcSECRET --log stdout");
        mask_secrets(&mut c, 0);
        assert_eq!(c, "ngrok http 80 --authtoken •••• --log stdout");
        let mut c = String::from("app --password=hunter2 API_KEY=xyz psql postgres://bob:pw@db/x --keyboard us");
        mask_secrets(&mut c, 0);
        assert_eq!(c, "app --password=•••• API_KEY=•••• psql postgres://bob:••••@db/x --keyboard us");
        let mut c = String::from("node server.js --port 3000");
        mask_secrets(&mut c, 0);
        assert_eq!(c, "node server.js --port 3000");
    }
}

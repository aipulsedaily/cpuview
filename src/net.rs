//! Network & system: network interfaces, sockets, containers and services.
//!
//! Everything is detected at run time and whatever is missing is simply absent: no
//! Docker (or Podman, LXC, nspawn / libvirt machines, Kubernetes pods), no systemd
//! (OpenRC services are found too), cgroup v1, no access to the Docker socket or to
//! other users' /proc entries, Wi-Fi only, IPv6 only, hundreds of interfaces, or
//! running inside a container.
//!
//! Costs (Ryzen 9950X): /proc/net/dev 40–55 µs here, because the r8169 driver reads
//! the NIC's hardware counters on every read (sysfs statistics cost the same, per
//! file), so it is read once per sample and only every 10 s while the page is hidden;
//! /proc/net/snmp 4 µs, sockstat 1 µs, a cgroup's cpu.stat 1 µs, memory.stat 2.6 µs,
//! a container's /proc/<pid>/net/dev 1.3 µs. A socket dump that isn't limited to
//! listening sockets walks the kernel's whole established-connection hash table
//! (~230 µs per address family), so that runs rarely, and only for a family that has
//! non-listening sockets (sockstat says). Expensive jobs run one per sample, spread.

use crate::sys::{isort, open_ro, parse_u64, pread, read_str, Ring};
use std::ffi::{CStr, CString};
use std::os::raw::c_int;

const SEC: i64 = 1_000_000_000;

/// Concatenate (paths: cheaper in code than format!).
pub fn cat(p: &[&str]) -> String {
    let mut s = String::with_capacity(96);
    for x in p {
        s += x;
    }
    s
}

fn dec(v: u64) -> String {
    let mut f = crate::ui::F::new();
    f.int(v as u32, 0);
    f.s().to_string()
}

/// Freed heap memory back to the kernel (after the rare big transient buffers).
fn trim() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

fn close(fd: c_int) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

/// Entries of a directory: (name, is a directory, inode). False if unreadable.
fn each_dir(path: &str, f: &mut dyn FnMut(&str, bool, u64)) -> bool {
    let Ok(c) = CString::new(path) else {
        return false;
    };
    let d = unsafe { libc::opendir(c.as_ptr()) };
    if d.is_null() {
        return false;
    }
    loop {
        let e = unsafe { libc::readdir(d) };
        if e.is_null() {
            break;
        }
        let e = unsafe { &*e };
        let n = unsafe { CStr::from_ptr(e.d_name.as_ptr()) };
        if let Ok(n) = n.to_str() {
            if !n.starts_with('.') {
                f(n, e.d_type == libc::DT_DIR, e.d_ino as u64);
            }
        }
    }
    unsafe { libc::closedir(d) };
    true
}

/// "key value" lines: the value of `key` (cgroup cpu.stat / memory.stat).
pub fn kv_val(b: &[u8], key: &[u8]) -> Option<u64> {
    for l in b.split(|&c| c == b'\n') {
        if l.len() > key.len() && l.starts_with(key) && l[key.len()] == b' ' {
            return Some(parse_u64(&l[key.len() + 1..]).0);
        }
    }
    None
}

fn pread_u64(fd: c_int, key: Option<&[u8]>) -> Option<u64> {
    let mut b = [0u8; 2048];
    let n = pread(fd, &mut b);
    if n == 0 {
        return None;
    }
    match key {
        Some(k) => kv_val(&b[..n], k),
        None => Some(parse_u64(&b[..n]).0),
    }
}

// ── interfaces ─────────────────────────────────────────────────────────────

pub const K_WIRED: u8 = 0;
pub const K_WIFI: u8 = 1;
pub const K_VPN: u8 = 2;
pub const K_BOND: u8 = 3;
pub const K_VLAN: u8 = 4;
pub const K_VIRT: u8 = 5;
pub const K_LO: u8 = 6;
pub const KIND: [&str; 7] = [
    "wired", "Wi-Fi", "VPN", "bond", "VLAN", "virtual", "loopback",
];

/// Interface kind from sysfs facts: `phys` (has a `device`), `wireless`, uevent
/// DEVTYPE, ARP hardware type, tun_flags & 3 (1 tun, 2 tap).
pub fn classify(name: &str, phys: bool, wireless: bool, devtype: &str, arp: u32, tun: u32) -> u8 {
    const VPN: [&str; 5] = ["wg", "tun", "tailscale", "zt", "ppp"];
    if arp == 772 || name == "lo" {
        K_LO
    } else if wireless || devtype == "wlan" {
        K_WIFI
    } else if devtype == "wireguard"
        || tun == 1
        || (tun == 0 && matches!(arp, 65534 | 512 | 768 | 769 | 776 | 778 | 823))
        || VPN.iter().any(|p| name.starts_with(p))
    {
        K_VPN
    } else if devtype == "bond" {
        K_BOND
    } else if devtype == "vlan" {
        K_VLAN
    } else if phys {
        K_WIRED
    } else {
        K_VIRT
    }
}

pub struct Iface {
    pub name: String,
    pub kind: u8,
    /// shown with its own rates (physical, VPN, bond / VLAN; or the only links there are)
    pub prim: bool,
    pub up: bool,
    /// carries the default route
    pub dflt: bool,
    /// link speed, Mbit/s (0 unknown)
    pub speed: u32,
    pub idx: u32,
    /// totals: received bytes, sent bytes, errors, drops
    pub cnt: [u64; 4],
    /// bytes/s now: received (download), sent (upload)
    pub rx: f32,
    pub tx: f32,
    /// when errors / drops last grew
    pub bad_t: i64,
    /// its address (a global one if it has one; IPv4 first)
    pub addr: String,
    addr_rank: u8,
    /// Wi-Fi signal (dBm, 0 unknown)
    pub sig: i32,
    /// per-second history: received, sent (primary links only)
    pub h: Option<Box<[Ring<f32>; 2]>>,
    /// the counters a rate is measured from (at least 0.45 s back)
    base: (i64, [u64; 2]),
    step: (i64, [u64; 2]),
    cur: (i64, [u64; 2]),
}

impl Iface {
    fn new(name: &str) -> Iface {
        Iface {
            name: name.to_string(),
            kind: K_VIRT,
            prim: false,
            up: false,
            dflt: false,
            speed: 0,
            idx: 0,
            cnt: [0; 4],
            rx: 0.0,
            tx: 0.0,
            bad_t: i64::MIN / 2,
            addr: String::new(),
            addr_rank: 9,
            sig: 0,
            h: None,
            base: (0, [0; 2]),
            step: (0, [0; 2]),
            cur: (0, [0; 2]),
        }
    }

    fn update(&mut self, now: i64, c: [u64; 4]) {
        if self.cur.0 == 0 {
            self.step = (now, [c[0], c[1]]);
        } else if c[2] + c[3] > self.cnt[2] + self.cnt[3] {
            self.bad_t = now;
        }
        self.cnt = c;
        self.cur = (now, [c[0], c[1]]);
        // rates over ≥ 0.45 s (smooth reads 5 times a second)
        let dt = (now - self.base.0) as f32 / 1e9;
        if self.base.0 == 0 {
            self.base = self.cur;
        } else if dt >= 0.45 {
            let d = |k: usize| c[k].saturating_sub(self.base.1[k]) as f32 / dt;
            (self.rx, self.tx) = (d(0), d(1));
            self.base = self.cur;
        }
    }

    /// Received / sent bytes/s over the unfinished history second(s).
    pub fn part(&self) -> (f32, f32) {
        let dt = (self.cur.0 - self.step.0) as f32 / 1e9;
        if dt <= 0.0 {
            return (self.rx, self.tx);
        }
        let d = |k: usize| self.cur.1[k].saturating_sub(self.step.1[k]) as f32 / dt;
        (d(0), d(1))
    }

    fn push(&mut self, times: u32) {
        let (r, t) = self.part();
        if let Some(h) = &mut self.h {
            for _ in 0..times {
                h[0].push(r);
                h[1].push(t);
            }
        }
        self.step = self.cur;
    }

    /// Errors or drops grew within the last minute.
    pub fn bad(&self, now: i64) -> bool {
        now - self.bad_t < 60 * SEC
    }
}

/// /proc/net/dev → (name, [rx bytes, tx bytes, rx+tx errors, rx+tx drops]) per line.
pub fn parse_dev(b: &[u8], f: &mut dyn FnMut(&str, [u64; 4])) {
    for l in b.split(|&c| c == b'\n').skip(2) {
        let Some(p) = l.iter().position(|&c| c == b':') else {
            continue;
        };
        let name = std::str::from_utf8(&l[..p]).unwrap_or("").trim();
        let mut v = [0u64; 12];
        let mut i = p + 1;
        for x in v.iter_mut() {
            let (n, k) = parse_u64(&l[i.min(l.len())..]);
            *x = n;
            i += k;
        }
        if !name.is_empty() {
            f(name, [v[0], v[8], v[2] + v[10], v[3] + v[11]]);
        }
    }
}

/// /proc/net/wireless → (name, signal dBm).
pub fn parse_wireless(b: &[u8], f: &mut dyn FnMut(&str, i32)) {
    for l in std::str::from_utf8(b).unwrap_or("").lines().skip(2) {
        let Some((n, rest)) = l.split_once(':') else {
            continue;
        };
        // status, link quality, signal level ("-54." or old-style 0..255 units)
        let mut it = rest.split_ascii_whitespace().skip(2);
        let s = it.next().unwrap_or("").trim_end_matches('.');
        let (neg, d) = match s.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, s),
        };
        let v = parse_u64(d.as_bytes()).0 as i32;
        let dbm = if neg {
            -v
        } else if v > 0 {
            v - 256
        } else {
            0
        };
        f(n.trim(), dbm);
    }
}

/// A file of /sys/class/net/<n>/ (trimmed; empty if unreadable).
#[inline(never)]
fn sysnet(n: &str, f: &str) -> String {
    read_str(&cat(&["/sys/class/net/", n, "/", f])).unwrap_or_default()
}

#[inline(never)]
fn sysnet_has(n: &str, f: &str) -> bool {
    std::path::Path::new(&cat(&["/sys/class/net/", n, "/", f])).exists()
}

/// Default-route interfaces from /proc/net/route (destination 0 and mask 0).
pub fn parse_route(t: &str) -> Vec<String> {
    let mut v = Vec::new();
    for l in t.lines().skip(1) {
        let f: Vec<&str> = l.split_ascii_whitespace().collect();
        if f.len() > 7 && f[1] == "00000000" && f[7] == "00000000" {
            v.push(f[0].to_string());
        }
    }
    v
}

/// The number after `key` in a line of words ("TCP: inuse 25 orphan 0 tw 3").
fn after(l: &str, key: &str) -> u32 {
    let mut it = l.split_ascii_whitespace();
    while let Some(w) = it.next() {
        if w == key {
            return parse_u64(it.next().unwrap_or("").as_bytes()).0 as u32;
        }
    }
    0
}

pub fn fmt_ip(a: &[u8; 16], v6: bool) -> String {
    let v4m = v6 && a[..10].iter().all(|&b| b == 0) && a[10] == 0xff && a[11] == 0xff;
    let mut s = String::with_capacity(40);
    if !v6 || v4m {
        let o = if v4m { 12 } else { 0 };
        for (i, b) in a[o..o + 4].iter().enumerate() {
            if i > 0 {
                s.push('.');
            }
            s += &dec(*b as u64);
        }
        return s;
    }
    // IPv6: eight hex groups, the longest run of zero groups (two or more) as "::"
    // (written here: libc's inet_ntop would map another 128 KB of libc)
    let g = |k: usize| (a[2 * k] as u32) << 8 | a[2 * k + 1] as u32;
    let (mut bs, mut bl, mut cl) = (8, 1, 0);
    for k in 0..8 {
        cl = if g(k) == 0 { cl + 1 } else { 0 };
        if cl > bl {
            (bs, bl) = (k + 1 - cl, cl);
        }
    }
    let mut k = 0;
    while k < 8 {
        if k == bs {
            s += "::";
            k += bl;
            continue;
        }
        if k > 0 && k != bs + bl {
            s.push(':');
        }
        let mut sh = 12;
        while sh > 0 && g(k) >> sh == 0 {
            sh -= 4;
        }
        loop {
            s.push(b"0123456789abcdef"[(g(k) >> sh & 15) as usize] as char);
            if sh == 0 {
                break;
            }
            sh -= 4;
        }
        k += 1;
    }
    s
}

// ── netlink ────────────────────────────────────────────────────────────────

/// One netlink dump: `req` (type, payload) → every reply message (type, payload).
fn nl_dump(
    fd: c_int,
    ty: u16,
    body: &[u8],
    buf: &mut Vec<u8>,
    f: &mut dyn FnMut(u16, &[u8]),
) -> bool {
    if fd < 0 {
        return false;
    }
    let mut req = [0u8; 128];
    let len = 16 + body.len();
    req[..4].copy_from_slice(&(len as u32).to_ne_bytes());
    req[4..6].copy_from_slice(&ty.to_ne_bytes());
    req[6..8].copy_from_slice(&0x301u16.to_ne_bytes()); // REQUEST | DUMP
    req[8..12].copy_from_slice(&7u32.to_ne_bytes());
    req[16..len].copy_from_slice(body);
    if unsafe { libc::send(fd, req.as_ptr() as *const _, len, 0) } != len as isize {
        return false;
    }
    // (the kernel sizes dump batches to the buffer we receive with)
    buf.resize(buf.len().max(8192), 0);
    loop {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut _, buf.len(), 0) };
        if n <= 0 {
            return false;
        }
        let b = &buf[..n as usize];
        let mut i = 0;
        while i + 16 <= b.len() {
            let ml = u32::from_ne_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            let mt = u16::from_ne_bytes(b[i + 4..i + 6].try_into().unwrap());
            if ml < 16 || i + ml > b.len() {
                return false;
            }
            match mt {
                3 => return true,  // DONE
                2 => return false, // ERROR
                _ => f(mt, &b[i + 16..i + ml]),
            }
            i += (ml + 3) & !3;
        }
    }
}

fn nl_socket(proto: c_int) -> c_int {
    unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            proto,
        )
    }
}

/// Netlink attributes (rtattr): (type, payload).
fn attrs(mut b: &[u8], f: &mut dyn FnMut(u16, &[u8])) {
    while b.len() >= 4 {
        let l = u16::from_ne_bytes([b[0], b[1]]) as usize;
        if l < 4 || l > b.len() {
            break;
        }
        f(u16::from_ne_bytes([b[2], b[3]]) & 0x3fff, &b[4..l]);
        b = &b[((l + 3) & !3).min(b.len())..];
    }
}

// ── sockets ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Sock {
    pub v6: bool,
    pub udp: bool,
    pub state: u8,
    pub la: [u8; 16],
    pub lp: u16,
    pub ra: [u8; 16],
    pub rp: u16,
    pub uid: u32,
    pub ino: u32,
    /// cgroup (v2) id: the inode of its directory
    pub cg: u64,
}

pub const ST_ESTAB: u8 = 1;
pub const ST_CLOSE: u8 = 7;
pub const ST_LISTEN: u8 = 10;

fn hexv(s: &[u8]) -> u64 {
    s.iter().fold(0, |a, &c| {
        a * 16
            + match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => 0,
            } as u64
    })
}

/// "0100007F:1F90" (or 32 hex digits for IPv6): the kernel's in-memory words.
fn proc_addr(s: &str, a: &mut [u8; 16]) -> u16 {
    let (h, p) = s.split_once(':').unwrap_or((s, "0"));
    for (k, w) in h.as_bytes().chunks(8).take(4).enumerate() {
        let v = hexv(w) as u32;
        a[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
    }
    hexv(p.as_bytes()) as u16
}

/// /proc/net/tcp, tcp6, udp, udp6 lines → sockets whose state is in `states` (bit
/// per state).
pub fn parse_proc_socks(t: &str, v6: bool, udp: bool, states: u32, out: &mut Vec<Sock>) {
    for l in t.lines().skip(1) {
        let f: Vec<&str> = l.split_ascii_whitespace().take(10).collect();
        if f.len() < 10 {
            continue;
        }
        let state = hexv(f[3].as_bytes()) as u8;
        if states & (1 << state) == 0 {
            continue;
        }
        let mut s = Sock {
            v6,
            udp,
            state,
            uid: f[7].parse().unwrap_or(0),
            ino: f[9].parse().unwrap_or(0),
            ..Default::default()
        };
        s.lp = proc_addr(f[1], &mut s.la);
        s.rp = proc_addr(f[2], &mut s.ra);
        out.push(s);
    }
}

/// inet_diag_msg (+ attributes) → socket.
fn diag_msg(p: &[u8], udp: bool) -> Option<Sock> {
    if p.len() < 72 {
        return None;
    }
    let u32at = |i: usize| u32::from_ne_bytes(p[i..i + 4].try_into().unwrap());
    let mut s = Sock {
        v6: p[0] == libc::AF_INET6 as u8,
        udp,
        state: p[1],
        lp: u16::from_be_bytes([p[4], p[5]]),
        rp: u16::from_be_bytes([p[6], p[7]]),
        uid: u32at(64),
        ino: u32at(68),
        ..Default::default()
    };
    s.la.copy_from_slice(&p[8..24]);
    s.ra.copy_from_slice(&p[24..40]);
    attrs(&p[72..], &mut |t, v| {
        if t == 21 && v.len() >= 8 {
            s.cg = u64::from_ne_bytes(v[..8].try_into().unwrap());
        }
    });
    Some(s)
}

#[inline]
fn is_local(a: &[u8; 16], v6: bool) -> bool {
    if !v6 {
        return a[0] == 127;
    }
    let z10 = a[..10].iter().all(|&b| b == 0);
    (z10 && a[10..15].iter().all(|&b| b == 0) && a[15] == 1)
        || (z10 && a[10] == 0xff && a[11] == 0xff && a[12] == 127)
}

fn is_any(a: &[u8; 16], v6: bool) -> bool {
    a[..if v6 { 16 } else { 4 }].iter().all(|&b| b == 0)
}

/// Listening ports, one entry per port.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listen {
    pub port: u16,
    /// 1 tcp, 2 udp, 3 both
    pub proto: u8,
    /// who can reach it: 0 every network, 1 this computer only, 2 one address
    pub scope: u8,
    pub addr: String,
    pub owner: String,
    pub inos: Vec<u32>,
    pub uid: u32,
    pub cg: u64,
}

/// Group listening TCP / unconnected UDP sockets by port.
pub fn group_listen(socks: &[Sock]) -> Vec<Listen> {
    let mut v: Vec<Listen> = Vec::new();
    for s in socks {
        if s.lp == 0 {
            continue;
        }
        let scope = if is_any(&s.la, s.v6) {
            0
        } else if is_local(&s.la, s.v6) {
            1
        } else {
            2
        };
        let i = match v.iter().position(|l| l.port == s.lp) {
            Some(i) => i,
            None => {
                v.push(Listen {
                    port: s.lp,
                    scope: 1,
                    uid: s.uid,
                    cg: s.cg,
                    ..Default::default()
                });
                v.len() - 1
            }
        };
        let l = &mut v[i];
        l.proto |= if s.udp { 2 } else { 1 };
        if scope == 0 || (scope == 2 && l.scope == 1) {
            l.scope = scope;
            if scope == 2 {
                l.addr = fmt_ip(&s.la, s.v6);
            }
        }
        if s.ino != 0 && !l.inos.contains(&s.ino) {
            l.inos.push(s.ino);
        }
    }
    isort(&mut v, &|a, b| a.port < b.port);
    v
}

/// Remote end of established connections, by count.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Peer {
    pub ip: [u8; 16],
    pub v6: bool,
    pub port: u16,
    pub n: u32,
    pub local: bool,
}

pub fn group_peers(socks: &[Sock], keep: usize) -> Vec<Peer> {
    // (hash of the remote end << 20 | socket index), sorted: equal ends are runs
    // (a busy server has thousands of connections: no quadratic grouping)
    let mut k: Vec<u64> = Vec::new();
    for (i, s) in socks.iter().enumerate().take(1 << 20) {
        if s.state == ST_ESTAB && !s.udp {
            let mut e = [0u8; 18];
            e[..16].copy_from_slice(&s.ra);
            e[16..].copy_from_slice(&s.rp.to_ne_bytes());
            k.push(hkey(&e) << 20 | i as u64);
        }
    }
    // (a shell sort: the library's sort costs ~3 KB of code)
    let mut gap = k.len() / 2;
    while gap > 0 {
        for i in gap..k.len() {
            let mut j = i;
            while j >= gap && k[j - gap] > k[j] {
                k.swap(j - gap, j);
                j -= gap;
            }
        }
        gap /= 2;
    }
    let mut v: Vec<Peer> = Vec::new();
    let mut i = 0;
    while i < k.len() {
        let mut j = i + 1;
        while j < k.len() && k[j] >> 20 == k[i] >> 20 {
            j += 1;
        }
        let s = &socks[(k[i] & 0xfffff) as usize];
        let n = (j - i) as u32;
        // keep the `keep` biggest, biggest first (then by port)
        let pos = v
            .iter()
            .position(|p| n > p.n || (n == p.n && s.rp < p.port));
        let p = Peer {
            ip: s.ra,
            v6: s.v6,
            port: s.rp,
            n,
            local: is_local(&s.ra, s.v6),
        };
        match pos {
            Some(q) => v.insert(q, p),
            None => v.push(p),
        }
        v.truncate(keep);
        i = j;
    }
    v
}

/// Well-known service names for a few ports.
pub fn port_name(p: u16) -> &'static str {
    match p {
        22 => "ssh",
        53 => "dns",
        80 => "web",
        443 => "https",
        3306 => "mysql",
        5432 => "postgres",
        _ => "",
    }
}

// ── tiny JSON field extractor (Docker / Podman API, their state files) ─────

/// The objects of a top-level JSON array (or the document, if it is an object).
pub fn jobjects(b: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    let (mut d, mut s, mut st, mut esc) = (0usize, 0usize, false, false);
    let top_obj = b.iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'{');
    for (i, &c) in b.iter().enumerate() {
        if st {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                st = false;
            }
            continue;
        }
        match c {
            b'"' => st = true,
            b'{' | b'[' => {
                d += 1;
                if c == b'{' && d == 2 - top_obj as usize {
                    s = i;
                }
            }
            b'}' | b']' => {
                if c == b'}' && d == 2 - top_obj as usize {
                    v.push(&b[s..=i]);
                }
                d = d.saturating_sub(1);
            }
            _ => {}
        }
    }
    v
}

/// Value of `"key":` in object `o`: at its top level only (`top`) or anywhere, from
/// byte `from`. Strings come without quotes (escapes kept), arrays as their first
/// string, other values as their text. Returns (value, index after it).
pub fn jget<'a>(o: &'a [u8], key: &str, top: bool, from: usize) -> Option<(&'a [u8], usize)> {
    let k = key.as_bytes();
    let (mut d, mut i) = (0usize, from);
    while i < o.len() {
        let c = o[i];
        if c == b'"' {
            let s = i + 1;
            let mut j = s;
            while j < o.len() && o[j] != b'"' {
                j += if o[j] == b'\\' { 2 } else { 1 };
            }
            let mut n = j + 1;
            while n < o.len() && o[n].is_ascii_whitespace() {
                n += 1;
            }
            if n < o.len() && o[n] == b':' && (!top || d == 1) && o.get(s..j) == Some(k) {
                return jval(o, n + 1);
            }
            i = j + 1;
            continue;
        }
        match c {
            b'{' | b'[' => d += 1,
            b'}' | b']' => d = d.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn jval(o: &[u8], mut i: usize) -> Option<(&[u8], usize)> {
    while i < o.len() && (o[i].is_ascii_whitespace() || o[i] == b'[') {
        i += 1;
    }
    if o.get(i) == Some(&b'"') {
        let s = i + 1;
        let mut j = s;
        while j < o.len() && o[j] != b'"' {
            j += if o[j] == b'\\' { 2 } else { 1 };
        }
        return Some((o.get(s..j)?, j + 1));
    }
    let s = i;
    while i < o.len() && !matches!(o[i], b',' | b'}' | b']') {
        i += 1;
    }
    Some((o[s..i].trim_ascii(), i))
}

fn jstr(o: &[u8], key: &str) -> String {
    jget(o, key, true, 0)
        .map(|v| String::from_utf8_lossy(v.0).into_owned())
        .unwrap_or_default()
}

/// A container as the Docker / Podman API lists it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApiCtr {
    pub id: String,
    pub name: String,
    /// 0 running, 1 restarting, 2 paused, 3 stopped
    pub state: u8,
    /// 0 no health check, 1 healthy, 2 starting, 3 unhealthy
    pub health: u8,
    pub ports: Vec<u16>,
}

pub fn parse_api(b: &[u8]) -> Vec<ApiCtr> {
    let mut out = Vec::new();
    for o in jobjects(b) {
        {
            let st = jstr(o, "State");
            let status = jstr(o, "Status");
            let mut c = ApiCtr {
                id: jstr(o, "Id"),
                name: jstr(o, "Names").trim_start_matches('/').to_string(),
                state: match st.as_str() {
                    "running" => 0,
                    "restarting" => 1,
                    "paused" => 2,
                    _ => 3,
                },
                health: if status.contains("(unhealthy)") {
                    3
                } else if status.contains("starting)") {
                    2
                } else {
                    status.contains("(healthy)") as u8
                },
                ..Default::default()
            };
            let mut i = 0;
            while let Some((v, n)) = jget(o, "PublicPort", false, i) {
                let p = parse_u64(v).0 as u16;
                if p > 0 && !c.ports.contains(&p) {
                    c.ports.push(p);
                }
                i = n;
            }
            if !c.id.is_empty() {
                out.push(c);
            }
        }
    }
    out
}

/// GET `path` over a unix socket (HTTP/1.0): the body of a 200 reply.
fn http_get(sock: &str, path: &str) -> Result<Vec<u8>, c_int> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(0);
        }
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 300_000,
        };
        for o in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                o,
                &tv as *const _ as *const _,
                std::mem::size_of::<libc::timeval>() as u32,
            );
        }
        let mut a: libc::sockaddr_un = std::mem::zeroed();
        a.sun_family = libc::AF_UNIX as u16;
        for (d, s) in a.sun_path.iter_mut().zip(sock.bytes().take(107)) {
            *d = s as libc::c_char;
        }
        if libc::connect(
            fd,
            &a as *const _ as *const _,
            std::mem::size_of_val(&a) as u32,
        ) != 0
        {
            let e = *libc::__errno_location();
            libc::close(fd);
            return Err(e);
        }
        let req = cat(&["GET ", path, " HTTP/1.0\r\nHost: cpuview\r\n\r\n"]);
        libc::write(fd, req.as_ptr() as *const _, req.len());
        let mut out = Vec::with_capacity(16384);
        let mut b = [0u8; 8192];
        loop {
            let n = libc::read(fd, b.as_mut_ptr() as *mut _, b.len());
            if n <= 0 || out.len() > 2 << 20 {
                break;
            }
            out.extend_from_slice(&b[..n as usize]);
        }
        libc::close(fd);
        let h = out.windows(4).position(|w| w == b"\r\n\r\n").ok_or(0)?;
        if !out[..h.min(16)].windows(5).any(|w| w == b" 200 ") {
            return Err(0);
        }
        Ok(out.split_off(h + 4))
    }
}

// ── cgroups: containers and services ───────────────────────────────────────

pub const C_DOCKER: u8 = 0;
pub const C_PODMAN: u8 = 1;
pub const C_K8S: u8 = 2;
pub const C_LXC: u8 = 3;
pub const C_VM: u8 = 4;
/// (the cgroup walk: a service, not a container)
const SVC: u8 = 255;

#[inline]
fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Is cgroup directory `name` (in `parent`) a container? → (kind, id or name).
pub fn ctr_dir(parent: &str, name: &str) -> Option<(u8, String)> {
    let base = name.strip_suffix(".scope").unwrap_or(name);
    for (p, k) in [
        ("docker-", C_DOCKER),
        ("libpod-", C_PODMAN),
        ("cri-containerd-", C_K8S),
        ("crio-", C_K8S),
    ] {
        if let Some(id) = base.strip_prefix(p) {
            return hex64(id).then(|| (k, id.to_string()));
        }
    }
    if let Some(n) = name.strip_prefix("lxc.payload.") {
        return Some((C_LXC, n.to_string()));
    }
    if parent == "lxc" && !name.starts_with("lxc.") {
        return Some((C_LXC, name.to_string()));
    }
    if let Some(n) = base.strip_prefix("machine-") {
        if name.ends_with(".scope") {
            return Some((C_VM, n.replace("\\x2d", "-")));
        }
    }
    if hex64(name) && (parent == "docker" || parent.starts_with("pod")) {
        return Some((
            if parent == "docker" { C_DOCKER } else { C_K8S },
            name.to_string(),
        ));
    }
    None
}

/// Cgroup directories worth looking into for containers.
fn descend(name: &str) -> bool {
    name.ends_with(".slice")
        || (name.starts_with("user@") && name.ends_with(".service"))
        || name.starts_with("pod")
        || matches!(
            name,
            "docker"
                | "lxc"
                | "kubepods"
                | "machine"
                | "libpod_parent"
                | "besteffort"
                | "burstable"
        )
}

/// Service in cgroup directory `name`: systemd system services, OpenRC services.
pub fn svc_dir(parent: &str, name: &str) -> Option<String> {
    if parent == "system.slice" {
        if let Some(n) = name.strip_suffix(".service") {
            return Some(n.to_string());
        }
    }
    if parent.is_empty() {
        if let Some(n) = name.strip_prefix("openrc.") {
            return Some(n.to_string());
        }
    }
    None
}

/// Where the cgroup statistics live: (v2, CPU hierarchy, memory hierarchy).
pub fn cg_roots(base: &str) -> (bool, String, String) {
    let ex = |p: &str| std::path::Path::new(p).exists();
    if ex(&cat(&[base, "/cgroup.controllers"])) {
        return (true, base.to_string(), base.to_string());
    }
    let cpu = ["/cpuacct", "/cpu,cpuacct", "/cpu"]
        .iter()
        .map(|d| cat(&[base, d]))
        .find(|p| ex(&cat(&[p, "/cpuacct.usage"])))
        .unwrap_or_default();
    (false, cpu, cat(&[base, "/memory"]))
}

/// Statistics files of a cgroup at `rel`: cpu, memory, memory.stat.
fn cg_open(v2: bool, cpu: &str, mem: &str, rel: &str) -> [c_int; 3] {
    let f: [(&str, &str); 3] = if v2 {
        [
            (cpu, "/cpu.stat"),
            (cpu, "/memory.current"),
            (cpu, "/memory.stat"),
        ]
    } else {
        [
            (cpu, "/cpuacct.usage"),
            (mem, "/memory.usage_in_bytes"),
            (mem, "/memory.stat"),
        ]
    };
    f.map(|(r, n)| open_ro(&cat(&[r, rel, n])))
}

/// A cgroup's CPU time (µs) and memory, read from its open files. With `stat`,
/// memory as `docker stats` counts it (less inactive page cache).
/// `stat`: 0 CPU only (memory u64::MAX), 1 memory.current, 2 less inactive cache.
fn cg_read(v2: bool, fd: &[c_int; 3], stat: u8) -> Option<(u64, u64)> {
    let cpu = if v2 {
        pread_u64(fd[0], Some(b"usage_usec"))?
    } else {
        pread_u64(fd[0], None)? / 1000
    };
    if stat == 0 {
        return Some((cpu, u64::MAX));
    }
    let mut mem = pread_u64(fd[1], None).unwrap_or(0);
    if stat == 2 {
        let k: &[u8] = if v2 {
            b"inactive_file"
        } else {
            b"total_inactive_file"
        };
        mem = mem.saturating_sub(pread_u64(fd[2], Some(k)).unwrap_or(0));
    }
    Some((cpu, mem))
}

/// CPU % (of one core) between two readings.
#[inline]
fn cpu_pct(prev: (i64, u64), now: i64, us: u64) -> f32 {
    let dt = (now - prev.0) as f32 / 1e3;
    if prev.0 == 0 || dt <= 0.0 {
        0.0
    } else {
        us.saturating_sub(prev.1) as f32 / dt * 100.0
    }
}

/// FNV-1a: list keys of names.
#[inline(never)]
pub fn hkey(s: &[u8]) -> u64 {
    s.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

pub struct Ctr {
    /// hkey of its cgroup directory's name
    pub k: u64,
    pub rel: String,
    pub id: String,
    pub name: String,
    pub kind: u8,
    pub state: u8,
    pub health: u8,
    /// cgroup files, the member's /proc/<pid>/net/dev
    fd: [c_int; 3],
    net: c_int,
    prev: (i64, u64),
    nprev: (i64, [u64; 2]),
    /// % of one core, bytes, bytes/s received / sent; network shared with the host
    pub cpu: f32,
    pub mem: f32,
    pub rx: f32,
    pub tx: f32,
    pub host_net: bool,
    pub ok: bool,
}

impl Drop for Ctr {
    fn drop(&mut self) {
        self.fd.iter().for_each(|&f| close(f));
        close(self.net);
    }
}

pub struct Svc {
    pub name: String,
    /// hkey(name)
    pub k: u64,
    fd: [c_int; 3],
    prev: (i64, u64),
    pub cpu: f32,
    pub mem: f32,
}

impl Drop for Svc {
    fn drop(&mut self) {
        self.fd.iter().for_each(|&f| close(f));
    }
}

impl Svc {
    /// Its statistics files opened and read once (CPU % needs a second reading).
    fn new(name: String, fd: [c_int; 3], v2: bool, now: i64) -> Svc {
        let mut s = Svc {
            k: hkey(name.as_bytes()),
            name,
            fd,
            prev: (0, 0),
            cpu: 0.0,
            mem: 0.0,
        };
        s.read(v2, now);
        s
    }

    fn read(&mut self, v2: bool, now: i64) {
        if let Some((us, m)) = cg_read(v2, &self.fd, 1) {
            self.cpu = cpu_pct(self.prev, now, us);
            self.prev = (now, us);
            self.mem = m as f32;
        }
    }
}

/// Program name of a process: argv[0]'s file name, else comm.
pub fn proc_name(pid: u32) -> String {
    let ps = dec(pid as u64);
    let c = std::fs::read(cat(&["/proc/", &ps, "/cmdline"])).unwrap_or_default();
    let a0 = c.split(|&b| b == 0 || b == b' ').next().unwrap_or(&[]);
    let base = a0.rsplit(|&b| b == b'/').next().unwrap_or(&[]);
    let n = String::from_utf8_lossy(base);
    let n = n.trim_end_matches(':');
    if n.is_empty() {
        read_str(&cat(&["/proc/", &ps, "/comm"])).unwrap_or_default()
    } else {
        n.to_string()
    }
}

/// Socket inode → owning program, found by looking through /proc/*/fd a few
/// processes per sample (only ever processes we may look into).
struct PidScan {
    dir: *mut libc::DIR,
    want: Vec<u32>,
}

// ── all of it ──────────────────────────────────────────────────────────────

// jobs: connection counts, container and service statistics (cheap), then listening
// ports, connections' remote ends, the cgroup walk, the engine API, links / addresses
const J_COUNTS: usize = 0;
const J_CTRS: usize = 1;
const J_SVCS: usize = 2;
const J_LISTEN: usize = 3;
const J_PEERS: usize = 4;
const J_WALK: usize = 5;
const J_API: usize = 6;
const J_LINKS: usize = 7;
const NJOB: usize = 8;
/// job periods (s): eco, other speeds
const PERIOD: [(i64, i64); NJOB] = [
    (4000, 1000),
    (10000, 1000),
    (30000, 2000),
    (15000, 5000),
    (60000, 15000),
    (20000, 10000),
    (60000, 30000),
    (60000, 30000),
];

pub struct Net {
    dev_fd: c_int,
    wl_fd: c_int,
    snmp_fd: c_int,
    ss_fd: [c_int; 2],
    diag_fd: c_int,
    buf: Vec<u8>,
    pub ifs: Vec<Iface>,
    /// virtual links (bridges, veth, ...) in a line: "docker0 · br-1 · 5 container links"
    pub virt: String,
    pub ndev: usize,
    pub hist_n: u64,
    /// history points pushed so far
    pub pts: u64,
    held: u32,
    // sockets
    pub estab: u32,
    pub tw: u32,
    /// TCP sockets in use per family (sockstat), listening per family
    inuse: [u32; 2],
    nlisten: [u32; 2],
    pub listen: Vec<Listen>,
    pub peers: Vec<Peer>,
    pub sock_ok: bool,
    socks: Vec<Sock>,
    /// socket inode → program (None: not ours to see / not found)
    owners: Vec<(u32, String)>,
    scan: Option<PidScan>,
    uid: u32,
    // cgroups
    pub cg2: bool,
    cpu_root: String,
    mem_root: String,
    pub ctrs: Vec<Ctr>,
    pub svcs: Vec<Svc>,
    /// logged-in users' processes (user.slice)
    pub user: Option<Svc>,
    /// cgroup id → unit name (sockets' owners)
    cgnames: Vec<(u64, String)>,
    // container engine API
    pub api_err: bool,
    pub api: Vec<ApiCtr>,
    pub stopped: u32,
    pub has_systemd: bool,
    last: [i64; NJOB],
    last_dev: i64,
    pub entered: bool,
    /// the overview's look-ups: containers only (no services, no cgroup names)
    pub lite: bool,
    cread: u32,
}

impl Net {
    pub fn empty() -> Net {
        Net {
            dev_fd: -1,
            wl_fd: -1,
            snmp_fd: -1,
            ss_fd: [-1; 2],
            diag_fd: -1,
            buf: Vec::new(),
            ifs: Vec::new(),
            virt: String::new(),
            ndev: 0,
            hist_n: 0,
            pts: 0,
            held: 0,
            estab: 0,
            tw: 0,
            inuse: [0; 2],
            nlisten: [0; 2],
            listen: Vec::new(),
            peers: Vec::new(),
            sock_ok: false,
            socks: Vec::new(),
            owners: Vec::new(),
            scan: None,
            uid: 0,
            cg2: false,
            cpu_root: String::new(),
            mem_root: String::new(),
            ctrs: Vec::new(),
            svcs: Vec::new(),
            user: None,
            cgnames: Vec::new(),
            api_err: false,
            api: Vec::new(),
            stopped: 0,
            has_systemd: false,
            last: [i64::MIN / 2; NJOB],
            last_dev: i64::MIN / 2,
            entered: false,
            lite: false,
            cread: 0,
        }
    }

    pub fn new() -> Net {
        let mut n = Net::empty();
        n.dev_fd = open_ro("/proc/net/dev");
        n.wl_fd = open_ro("/proc/net/wireless");
        n.snmp_fd = open_ro("/proc/net/snmp");
        n.ss_fd = [
            open_ro("/proc/net/sockstat"),
            open_ro("/proc/net/sockstat6"),
        ];
        n.buf = vec![0; 8192];
        n.uid = unsafe { libc::geteuid() };
        (n.cg2, n.cpu_root, n.mem_root) = cg_roots("/sys/fs/cgroup");
        n.has_systemd = std::path::Path::new("/run/systemd/system").exists();
        n.links();
        n.read_dev(crate::term::mono());
        n
    }

    /// The page just came into view: everything fresh at once (input handling,
    /// outside the eco budget).
    pub fn enter(&mut self, now: i64) {
        self.entered = true;
        self.lite = false;
        self.read_counts();
        for j in 0..NJOB {
            self.job(j, now);
            self.last[j] = now - j as i64 * SEC / 3;
        }
        self.read_dev(now);
        // CPU % needs a second reading: soon
        self.last[J_CTRS] = now - 8 * SEC;
        self.last[J_SVCS] = now - 28 * SEC;
        while self.scan.is_some() {
            self.scan_step(64);
        }
        self.resolve_owners();
        trim();
    }

    /// One sample. `vis`: the page is shown; `eco`: the adaptive low-CPU speed.
    pub fn tick(&mut self, now: i64, push: u32, vis: bool, eco: bool) {
        self.held += push;
        // (smooth: 5 times a second is plenty, the rates are 0.5 s windows)
        let dev_gap = if !vis {
            10 * SEC
        } else if self.ndev > 64 {
            5 * SEC
        } else if eco {
            0
        } else {
            SEC / 5
        };
        // (a new second's history point: always on time while shown)
        if now - self.last_dev >= dev_gap * 9 / 10 || (vis && push > 0) {
            self.read_dev(now);
        }
        if !vis {
            return;
        }
        if !self.entered {
            self.enter(now);
            return;
        }
        // the jobs: in eco one per sample (the most overdue first, a pending look
        // for sockets' owners before them), so no sample costs much; at other speeds
        // the cheap ones whenever due, and one other
        let (nc, ns) = (self.ctrs.len() as i64, self.svcs.len() as i64);
        if self.scan.is_some() {
            self.scan_step(24);
            if self.scan.is_none() {
                self.resolve_owners();
            }
            if eco {
                return;
            }
        }
        let mut best = None;
        for (j, p) in PERIOD.iter().enumerate() {
            let p = (if eco { p.0 } else { p.1 }) * 1_000_000;
            let p = match j {
                J_CTRS if nc == 0 => continue,
                J_SVCS if ns == 0 => continue,
                J_CTRS => p.max(nc * SEC / 8),
                J_SVCS => p.max(ns * SEC / 16),
                _ => p,
            };
            let late = now - self.last[j] - p;
            if late <= -SEC / 5 {
                continue;
            }
            if !eco && j <= J_SVCS {
                self.last[j] = now;
                self.job(j, now);
            } else if best.is_none_or(|(_, l)| late > l) {
                best = Some((j, late));
            }
        }
        if let Some((j, _)) = best {
            self.last[j] = now;
            self.job(j, now);
        }
    }

    /// The overview (view 0): the links' rates every `gap` (eco 10 s: a read of
    /// /proc/net/dev can cost 50 µs), the containers (cgroup walk, the engine API's
    /// names and health) every 5 / 2 minutes (eco; else 60 / 30 s); no sockets, no service statistics.
    /// `enter`: everything now (the page just came into view).
    /// Returns the CPU time (ns) of the links' read and of the container jobs.
    pub fn tick_ov(&mut self, now: i64, push: u32, eco: bool, enter: bool) -> (i64, i64) {
        self.held += push;
        // (the network page, shown later, looks everything up afresh)
        self.lite = true;
        self.entered = false;
        let gap = if eco { 10 * SEC } else { SEC };
        let t0 = crate::term::cpu_ns();
        if enter || now - self.last_dev >= gap - SEC / 5 {
            self.read_dev(now);
        }
        // (one job a sample)
        let t1 = crate::term::cpu_ns();
        for (j, p) in [(J_WALK, if eco { 300 } else { 60 }), (J_API, if eco { 120 } else { 30 })] {
            if enter || (now - self.last[j] >= p * SEC - SEC / 5 && (j == J_WALK || !self.ctrs.is_empty())) {
                self.last[j] = now;
                self.job(j, now);
                if !enter {
                    break;
                }
            }
        }
        (t1 - t0, crate::term::cpu_ns() - t1)
    }

    fn job(&mut self, j: usize, now: i64) {
        match j {
            J_COUNTS => self.read_counts(),
            J_CTRS => self.read_ctrs(now),
            J_SVCS => self.read_svcs(now),
            J_LISTEN => self.read_listen(),
            J_PEERS => self.read_peers(),
            J_WALK => self.walk(now),
            J_API => self.read_api(),
            J_LINKS => {
                self.links();
                self.addrs();
            }
            _ => {}
        }
        // the big transient buffers (socket tables, the API's JSON) go back to the
        // kernel: these jobs are rare
        if j >= J_PEERS {
            trim();
        }
    }

    // ── interfaces ─────────────────────────────────────────────────────────

    fn read_dev(&mut self, now: i64) {
        self.last_dev = now;
        let mut n = pread(self.dev_fd, &mut self.buf);
        while n == self.buf.len() && n < 1 << 20 {
            // hundreds of interfaces: a bigger buffer
            self.buf.resize(2 * n, 0);
            n = pread(self.dev_fd, &mut self.buf);
        }
        let mut cnt = 0;
        let mut fresh = false;
        let ifs = &mut self.ifs;
        parse_dev(&self.buf[..n], &mut |name, c| {
            cnt += 1;
            match ifs.iter_mut().find(|i| i.name == name) {
                Some(i) if i.prim => i.update(now, c),
                Some(_) => {}
                None => fresh = true,
            }
        });
        if cnt != self.ndev || fresh {
            self.ndev = cnt;
            self.links();
        }
        let push = std::mem::take(&mut self.held);
        if push > 0 {
            for i in &mut self.ifs {
                i.push(push);
            }
            self.hist_n += 1;
            self.pts += push as u64;
        }
        if self.ifs.iter().any(|i| i.kind == K_WIFI && i.up) {
            let n = pread(self.wl_fd, &mut self.buf);
            let ifs = &mut self.ifs;
            parse_wireless(&self.buf[..n], &mut |name, s| {
                if let Some(i) = ifs.iter_mut().find(|i| i.name == name) {
                    i.sig = s;
                }
            });
        }
    }

    /// (Re)discover interfaces: kinds, state, speed, default route, order.
    fn links(&mut self) {
        let mut names = Vec::new();
        each_dir("/sys/class/net", &mut |n, _, _| names.push(n.to_string()));
        self.ifs.retain(|i| names.contains(&i.name));
        let dflt = parse_route(&std::fs::read_to_string("/proc/net/route").unwrap_or_default());
        for n in &names {
            let rd = |f: &str| sysnet(n, f);
            let ex = |f: &str| sysnet_has(n, f);
            if !self.ifs.iter().any(|i| &i.name == n) {
                let mut i = Iface::new(n);
                let ue = rd("uevent");
                let dt = ue
                    .lines()
                    .find_map(|l| l.strip_prefix("DEVTYPE="))
                    .unwrap_or("");
                let tun = if ex("tun_flags") {
                    (hexv(rd("tun_flags").as_bytes()) & 3) as u32
                } else {
                    0
                };
                i.kind = classify(
                    n,
                    ex("device"),
                    ex("wireless") || ex("phy80211"),
                    dt,
                    parse_u64(rd("type").as_bytes()).0 as u32,
                    tun,
                );
                i.idx = parse_u64(rd("ifindex").as_bytes()).0 as u32;
                self.ifs.push(i);
            }
            let i = self.ifs.iter_mut().find(|i| &i.name == n).unwrap();
            let op = rd("operstate");
            i.up = op == "up" || (op == "unknown" && rd("carrier") == "1");
            // (unknown speed reads "-1": 0 here)
            i.speed = if i.up {
                parse_u64(rd("speed").as_bytes()).0.min(1 << 30) as u32
            } else {
                0
            };
            i.dflt = dflt.contains(n);
        }
        let any = self.ifs.iter().any(|i| i.kind < K_VIRT);
        let up = self.ifs.iter().any(|i| i.kind < K_VIRT && i.up);
        for i in &mut self.ifs {
            // no physical link (a container, a VM with odd drivers): the virtual ones
            // that are up are the network
            i.prim = i.kind < K_VIRT || (!up && i.kind == K_VIRT && (i.up || !any));
            // history for the links that are up (8 KB each)
            if i.prim && i.up && i.h.is_none() {
                i.h = Some(Box::new([Ring::new(), Ring::new()]));
            }
            if !i.prim {
                i.h = None;
            }
        }
        let rank = |i: &Iface| {
            (!i.prim as u8) << 3
                | (!i.dflt as u8) << 2
                | (!i.up as u8) << 1
                | (i.kind == K_VPN) as u8
        };
        isort(&mut self.ifs, &|a, b| {
            (rank(a), &a.name) < (rank(b), &b.name)
        });
        self.ifs.truncate(4096);
        self.virt.clear();
        let mut nveth = 0;
        for i in self.ifs.iter().filter(|i| !i.prim && i.kind == K_VIRT) {
            if i.name.starts_with("veth") {
                nveth += 1;
            } else {
                self.virt += &i.name;
                self.virt += " · ";
            }
        }
        if nveth > 0 {
            self.virt += &dec(nveth);
            self.virt += " container links";
        } else if !self.virt.is_empty() {
            self.virt.truncate(self.virt.len() - " · ".len());
        }
    }

    /// Addresses of every interface (netlink RTM_GETADDR: no counters touched, and
    /// no libc getifaddrs, which maps another 128 KB of libc): a global one first
    /// (IPv4, then IPv6), a link-local one only when there is nothing else.
    fn addrs(&mut self) {
        let fd = nl_socket(libc::NETLINK_ROUTE);
        for i in &mut self.ifs {
            i.addr_rank = 9;
            i.addr.clear();
        }
        let mut buf = std::mem::take(&mut self.buf);
        let ifs = &mut self.ifs;
        nl_dump(fd, 22, &[0u8; 8], &mut buf, &mut |t, p| {
            if t != 20 || p.len() < 8 {
                return;
            }
            let (v6, scope) = (p[0] == libc::AF_INET6 as u8, p[3]);
            let idx = u32::from_ne_bytes([p[4], p[5], p[6], p[7]]);
            let mut a = [0u8; 16];
            let mut have = false;
            attrs(&p[8..], &mut |at, v| {
                // IFA_LOCAL (2) wins over IFA_ADDRESS (1) for IPv4 point-to-point links
                if (at == 2 || (at == 1 && !have)) && (v.len() == 4 || v.len() == 16) {
                    a[..v.len()].copy_from_slice(v);
                    have = true;
                }
            });
            let rank = (scope == 253) as u8 * 2 + v6 as u8;
            if let Some(i) = ifs.iter_mut().find(|i| i.idx == idx && i.prim) {
                if have && scope != 254 && rank < i.addr_rank {
                    i.addr_rank = rank;
                    i.addr = fmt_ip(&a, v6);
                }
            }
        });
        self.buf = buf;
        close(fd);
    }

    // ── sockets ────────────────────────────────────────────────────────────

    /// Connection counts: open (ESTABLISHED + CLOSE_WAIT, /proc/net/snmp), closing
    /// (TIME_WAIT) and TCP sockets in use per family (sockstat): O(1) reads.
    fn read_counts(&mut self) {
        let n = pread(self.snmp_fd, &mut self.buf);
        let t = std::str::from_utf8(&self.buf[..n]).unwrap_or("");
        // "Tcp: … CurrEstab …" then "Tcp: … <value> …"
        let mut col = None;
        for l in t.lines() {
            if let Some(r) = l.strip_prefix("Tcp:") {
                match col {
                    None => col = r.split_ascii_whitespace().position(|x| x == "CurrEstab"),
                    Some(k) => {
                        let v = r.split_ascii_whitespace().nth(k).unwrap_or("");
                        self.estab = parse_u64(v.as_bytes()).0 as u32;
                        break;
                    }
                }
            }
        }
        for f in 0..2 {
            let n = pread(self.ss_fd[f], &mut self.buf);
            let t = std::str::from_utf8(&self.buf[..n]).unwrap_or("");
            for l in t.lines() {
                if l.starts_with(["TCP:", "TCP6:"][f]) {
                    self.inuse[f] = after(l, "inuse");
                    if f == 0 {
                        self.tw = after(l, "tw");
                    }
                }
            }
        }
    }

    /// Sockets in `states` of one family / protocol: sock_diag netlink, else /proc.
    fn dump(&mut self, v6: bool, udp: bool, states: u32) {
        if self.diag_fd == -1 {
            self.diag_fd = nl_socket(libc::NETLINK_SOCK_DIAG);
            if self.diag_fd < 0 {
                self.diag_fd = -2;
            }
        }
        let mut req = [0u8; 56];
        req[0] = if v6 { libc::AF_INET6 } else { libc::AF_INET } as u8;
        req[1] = if udp { 17 } else { 6 };
        req[4..8].copy_from_slice(&states.to_ne_bytes());
        let mut buf = std::mem::take(&mut self.buf);
        let mut got = std::mem::take(&mut self.socks);
        let n0 = got.len();
        let ok = nl_dump(self.diag_fd, 20, &req, &mut buf, &mut |_, p| {
            if let Some(s) = diag_msg(p, udp) {
                got.push(s);
            }
        });
        if !ok {
            got.truncate(n0);
            let f = cat(&[
                "/proc/net/",
                if udp { "udp" } else { "tcp" },
                if v6 { "6" } else { "" },
            ]);
            parse_proc_socks(
                &std::fs::read_to_string(f).unwrap_or_default(),
                v6,
                udp,
                states,
                &mut got,
            );
        }
        self.buf = buf;
        self.socks = got;
    }

    fn read_listen(&mut self) {
        self.socks.clear();
        for v6 in [false, true] {
            self.dump(v6, false, 1 << ST_LISTEN);
            let n = self.socks.iter().filter(|s| s.v6 == v6).count() as u32;
            self.nlisten[v6 as usize] = n;
        }
        for v6 in [false, true] {
            self.dump(v6, true, 1 << ST_CLOSE);
        }
        self.socks.retain(|s| !s.udp || s.rp == 0);
        self.sock_ok = true;
        self.listen = group_listen(&self.socks);
        // owners: what we can't see as ours is looked up a few processes at a time
        let mine = |uid: u32| self.uid == 0 || uid == self.uid;
        let mut want: Vec<u32> = Vec::new();
        for l in &self.listen {
            for &i in &l.inos {
                if mine(l.uid) && !self.owners.iter().any(|o| o.0 == i) {
                    want.push(i);
                }
            }
        }
        self.owners
            .retain(|o| self.socks.iter().any(|s| s.ino == o.0));
        if !want.is_empty() && self.scan.is_none() {
            let dir = unsafe { libc::opendir(c"/proc".as_ptr()) };
            if !dir.is_null() {
                self.scan = Some(PidScan { dir, want });
            }
        }
        self.resolve_owners();
    }

    /// Look through the fds of up to `n` processes for the sockets wanted.
    fn scan_step(&mut self, n: usize) {
        let Some(sc) = &mut self.scan else { return };
        let mut done = false;
        for _ in 0..n {
            let e = unsafe { libc::readdir(sc.dir) };
            if e.is_null() {
                done = true;
                break;
            }
            let name = unsafe { CStr::from_ptr((*e).d_name.as_ptr()) };
            let Ok(pid) = name.to_str().unwrap_or("").parse::<u32>() else {
                continue;
            };
            let mut hits = Vec::new();
            let ps = dec(pid as u64);
            each_dir(&cat(&["/proc/", &ps, "/fd"]), &mut |fd, _, _| {
                let mut p = cat(&["/proc/", &ps, "/fd/", fd]).into_bytes();
                p.push(0);
                let mut b = [0u8; 64];
                let l =
                    unsafe { libc::readlink(p.as_ptr() as *const _, b.as_mut_ptr() as *mut _, 64) };
                if l > 9 && b.starts_with(b"socket:[") {
                    let ino = parse_u64(&b[8..l as usize]).0 as u32;
                    if sc.want.contains(&ino) {
                        hits.push(ino);
                    }
                }
            });
            if !hits.is_empty() {
                let nm = proc_name(pid);
                for h in hits {
                    sc.want.retain(|w| *w != h);
                    self.owners.push((h, nm.clone()));
                }
                if sc.want.is_empty() {
                    done = true;
                    break;
                }
            }
        }
        if done {
            for w in &sc.want {
                self.owners.push((*w, String::new()));
            }
            unsafe { libc::closedir(sc.dir) };
            self.scan = None;
            trim();
        }
    }

    /// Who owns each listening port: a container (published port), the program
    /// (when we may see it), the systemd unit / cgroup it was opened in, else "?".
    fn resolve_owners(&mut self) {
        for l in &mut self.listen {
            let ctr = self.api.iter().find(|c| c.ports.contains(&l.port));
            let prog = l
                .inos
                .iter()
                .find_map(|i| self.owners.iter().find(|o| o.0 == *i && !o.1.is_empty()))
                .map(|o| o.1.clone());
            let unit = self.cgnames.iter().find(|c| c.0 == l.cg && l.cg != 0);
            l.owner = if let Some(c) = ctr {
                c.name.clone()
            } else if let Some(p) = prog {
                p
            } else if let Some(u) = unit {
                let n = &u.1;
                n.rsplit_once('.').map_or(n.as_str(), |s| s.0).to_string()
            } else {
                "?".into()
            };
        }
    }

    /// Established connections' remote ends: a walk of the kernel's connection
    /// table per family (skipped when that family has only listening sockets).
    fn read_peers(&mut self) {
        self.socks.clear();
        // (/proc, not sock_diag: here a netlink dump of all connections costs 3.8 ms,
        // /proc/net/tcp 0.3 ms)
        for (f, p) in ["/proc/net/tcp", "/proc/net/tcp6"].iter().enumerate() {
            if self.inuse[f] > self.nlisten[f] || !self.sock_ok {
                let t = std::fs::read_to_string(p).unwrap_or_default();
                parse_proc_socks(&t, f == 1, false, 1 << ST_ESTAB, &mut self.socks);
            }
        }
        self.peers = group_peers(&self.socks, 12);
        self.socks = Vec::new();
    }

    // ── containers and services ────────────────────────────────────────────

    /// Find containers and services in the cgroup tree (only the branches that
    /// can hold them are read).
    fn walk(&mut self, now: i64) {
        if self.cpu_root.is_empty() {
            return;
        }
        let lite = self.lite;
        let mut found: Vec<(String, u8, String)> = Vec::new();
        let mut names: Vec<(u64, String)> = Vec::new();
        let mut stack = vec![String::new()];
        let mut dirs = 0;
        while let Some(rel) = stack.pop() {
            dirs += 1;
            if dirs > 400 {
                break;
            }
            let depth = rel.bytes().filter(|&b| b == b'/').count();
            let parent = rel.rsplit('/').next().unwrap_or("").to_string();
            each_dir(&cat(&[&self.cpu_root, &rel]), &mut |n, d, ino| {
                if !d {
                    return;
                }
                let path = cat(&[&rel, "/", n]);
                if names.len() < 4096 && !lite {
                    names.push((ino, n.to_string()));
                }
                if let Some((k, id)) = ctr_dir(&parent, n) {
                    found.push((path, k, id));
                } else if let Some(s) = svc_dir(&parent, n).filter(|_| !lite) {
                    found.push((path, SVC, s));
                } else if depth < 6 && descend(n) {
                    stack.push(path);
                }
            });
        }
        if !lite {
            self.cgnames = names;
        }
        let (v2, cr, mr) = (self.cg2, self.cpu_root.clone(), self.mem_root.clone());
        // containers: keep the known ones (their counters), add the new
        self.ctrs.retain(|c| found.iter().any(|f| f.0 == c.rel));
        self.svcs
            .retain(|s| found.iter().any(|f| f.1 == SVC && f.2 == s.name));
        for (rel, kind, id) in found {
            if kind == SVC {
                if !self.svcs.iter().any(|s| s.name == id) && self.svcs.len() < 160 {
                    self.svcs
                        .push(Svc::new(id, cg_open(v2, &cr, &mr, &rel), v2, now));
                }
                continue;
            }
            if self.ctrs.iter().any(|c| c.rel == rel) || self.ctrs.len() >= 256 {
                continue;
            }
            let key = rel.rsplit('/').next().unwrap_or("").to_string();
            let short = if hex64(&id) {
                id[..12].to_string()
            } else {
                id.clone()
            };
            self.ctrs.push(Ctr {
                k: hkey(key.as_bytes()),
                fd: cg_open(v2, &cr, &mr, &rel),
                net: -1,
                rel,
                id,
                name: short,
                kind,
                state: 0,
                health: 0,
                prev: (0, 0),
                nprev: (0, [0; 2]),
                cpu: 0.0,
                mem: 0.0,
                rx: 0.0,
                tx: 0.0,
                host_net: false,
                ok: false,
            });
        }
        let mut host: Vec<String> = Vec::new();
        for i in &self.ifs {
            if i.prim {
                host.push(i.name.clone());
            }
        }
        for c in &mut self.ctrs {
            if c.net < 0 {
                c.net = ctr_net(&cr, &c.rel, &host, &mut c.host_net);
            }
        }
        self.apply_api();
        if self
            .ctrs
            .iter()
            .any(|c| hex64(&c.id) && !self.api.iter().any(|a| a.id == c.id))
        {
            self.last[J_API] = i64::MIN / 2; // a new container: ask for its name soon
        }
        // the logged-in users' total
        if self.user.is_none() && !self.svcs.is_empty() {
            let fd = cg_open(v2, &cr, &mr, "/user.slice");
            if fd[0] >= 0 {
                self.user = Some(Svc::new("user.slice".into(), fd, v2, now));
            }
        }
        self.resolve_owners();
    }

    fn read_ctrs(&mut self, now: i64) {
        let v2 = self.cg2;
        let mut b = [0u8; 4096];
        // memory (memory.stat costs the most) every other time
        self.cread += 1;
        let stat = if self.cread % 2 == 1 { 2 } else { 0 };
        for c in &mut self.ctrs {
            if let Some((us, m)) = cg_read(v2, &c.fd, stat) {
                c.cpu = cpu_pct(c.prev, now, us);
                c.prev = (now, us);
                if m != u64::MAX {
                    c.mem = m as f32;
                    c.ok = true;
                }
            }
            if c.net >= 0 && !c.host_net {
                let n = pread(c.net, &mut b);
                if n == 0 {
                    close(c.net);
                    c.net = -1; // its process is gone: found again on the next walk
                    continue;
                }
                let mut t = [0u64; 2];
                parse_dev(&b[..n], &mut |name, v| {
                    if name != "lo" {
                        t[0] += v[0];
                        t[1] += v[1];
                    }
                });
                let dt = (now - c.nprev.0) as f32 / 1e9;
                if c.nprev.0 != 0 && dt > 0.0 {
                    c.rx = t[0].saturating_sub(c.nprev.1[0]) as f32 / dt;
                    c.tx = t[1].saturating_sub(c.nprev.1[1]) as f32 / dt;
                }
                c.nprev = (now, t);
            }
        }
    }

    fn read_svcs(&mut self, now: i64) {
        let v2 = self.cg2;
        for s in self.svcs.iter_mut().chain(self.user.as_mut()) {
            s.read(v2, now);
        }
    }

    /// Names, state and health from the Docker or Podman API (whichever socket we
    /// may use), else from state files we can read, else the short id.
    fn read_api(&mut self) {
        self.api.clear();
        if self.ctrs.is_empty() {
            return;
        }
        let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        let (p, d) = (
            cat(&[&rt, "/podman/podman.sock"]),
            cat(&[&rt, "/docker.sock"]),
        );
        let mut ok = false;
        self.api_err = false;
        for s in ["/var/run/docker.sock", "/run/podman/podman.sock", &p, &d] {
            match http_get(s, "/containers/json?all=1") {
                Ok(b) => {
                    self.api = parse_api(&b);
                    ok = true;
                    break;
                }
                Err(e) => self.api_err |= e == libc::EACCES,
            }
        }
        if ok {
            self.api_err = false;
        } else {
            {
                // Docker's state files (readable by root, usually)
                for c in &self.ctrs {
                    if c.kind == C_DOCKER {
                        let p = cat(&["/var/lib/docker/containers/", &c.id, "/config.v2.json"]);
                        if let Ok(b) = std::fs::read(p) {
                            self.api.push(ApiCtr {
                                id: c.id.clone(),
                                name: jstr(&b, "Name").trim_start_matches('/').to_string(),
                                state: 9,
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }
        self.apply_api();
        self.resolve_owners();
    }

    fn apply_api(&mut self) {
        self.stopped = self.api.iter().filter(|a| a.state == 3).count() as u32;
        for c in &mut self.ctrs {
            if let Some(a) = self.api.iter().find(|a| a.id == c.id) {
                if !a.name.is_empty() {
                    c.name.clone_from(&a.name);
                }
                if a.state != 9 {
                    c.state = a.state;
                    c.health = a.health;
                }
            }
        }
    }

    /// Primary links: (how many, how many up, the first up).
    pub fn links_up(&self) -> (usize, usize, Option<usize>) {
        let (mut n, mut up, mut first) = (0, 0, None);
        for (k, i) in self.ifs.iter().enumerate() {
            if i.prim {
                n += 1;
                if i.up {
                    up += 1;
                    first = first.or(Some(k));
                }
            }
        }
        (n, up, first)
    }

    /// A fake set for tests and layout checks.
    #[cfg(test)]
    pub fn fake(nctr: usize, nsvc: usize) -> Net {
        let mut n = Net::empty();
        for (k, nm) in ["eno1", "eno2", "wlp8s0"].iter().enumerate() {
            let mut i = Iface::new(nm);
            i.kind = if k == 2 { K_WIFI } else { K_WIRED };
            i.prim = true;
            i.up = k == 0;
            i.dflt = k == 0;
            i.speed = 1000;
            i.addr = "10.0.0.15".into();
            let mut h = Box::new([Ring::new(), Ring::new()]);
            for s in 0..300 {
                h[0].push((s * 7919 % 1000) as f32 * 1e4);
                h[1].push((s * 31 % 100) as f32 * 1e3);
            }
            i.h = Some(h);
            i.rx = 1.2e6;
            i.tx = 3.4e4;
            n.ifs.push(i);
        }
        n.virt = "docker0 · 1 container links".into();
        n.listen = (0..12)
            .map(|k| Listen {
                port: 22 + k as u16 * 1000,
                proto: 1,
                scope: (k % 3) as u8,
                addr: "10.0.0.15".into(),
                owner: "sshd".into(),
                ..Default::default()
            })
            .collect();
        n.peers = (0..6)
            .map(|k| Peer {
                port: 443,
                n: 10 - k,
                ..Default::default()
            })
            .collect();
        n.sock_ok = true;
        for k in 0..nctr {
            n.ctrs.push(Ctr {
                k: k as u64,
                rel: String::new(),
                id: String::new(),
                name: format!("container-{k}"),
                kind: 0,
                state: 0,
                health: (k % 4) as u8,
                fd: [-1; 3],
                net: -1,
                prev: (0, 0),
                nprev: (0, [0; 2]),
                cpu: k as f32 * 3.0,
                mem: k as f32 * 1e8,
                rx: 1e3,
                tx: 2e3,
                host_net: false,
                ok: true,
            });
        }
        for k in 0..nsvc {
            n.svcs.push(Svc {
                k: 100 + k as u64,
                name: format!("service{k}"),
                fd: [-1; 3],
                prev: (0, 0),
                cpu: k as f32,
                mem: k as f32 * 1e7,
            });
        }
        n.entered = true;
        n
    }
}

/// Open a container's network counters: /proc/<pid>/net/dev of a member process
/// (its cgroup.procs, or a child cgroup's for LXC). `host`: it shares the host's
/// network (it lists one of our physical links).
fn ctr_net(root: &str, rel: &str, host: &[String], host_net: &mut bool) -> c_int {
    // a member process: its cgroup's first, else (LXC) a child cgroup's
    let base = cat(&[root, rel]);
    let mut dirs = vec![base.clone()];
    each_dir(&base, &mut |n, d, _| {
        if d && dirs.len() < 5 {
            dirs.push(cat(&[&base, "/", n]));
        }
    });
    let mut pid = 0;
    for d in &dirs {
        if pid == 0 {
            pid = parse_u64(&std::fs::read(cat(&[d, "/cgroup.procs"])).unwrap_or_default()).0;
        }
    }
    if pid == 0 {
        return -1;
    }
    let fd = open_ro(&cat(&["/proc/", &dec(pid), "/net/dev"]));
    let mut b = [0u8; 4096];
    let n = pread(fd, &mut b);
    *host_net = false;
    parse_dev(&b[..n], &mut |name, _| {
        if host.iter().any(|h| h == name) {
            *host_net = true;
        }
    });
    fd
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 437667401165 267590006    0    0    0     0          0         0 437667401165 267590006    0    0    0     0       0          0
  eno1: 60336507436 43173883    0 62177    0     0          0    151119 6482968285 4674499    0    0    0     0       0          0
wlp8s0:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0
vethbf44beb: 38308865097  677684    1    0    0     0          0         0 112749363  190125    2    3    0     0       0          0
";

    #[test]
    fn proc_net_dev() {
        let mut v = Vec::new();
        parse_dev(DEV.as_bytes(), &mut |n, c| v.push((n.to_string(), c)));
        assert_eq!(v.len(), 4);
        assert_eq!(v[1], ("eno1".into(), [60336507436, 6482968285, 0, 62177]));
        assert_eq!(v[3], ("vethbf44beb".into(), [38308865097, 112749363, 3, 3]));
    }

    #[test]
    fn rates_follow_counter_deltas() {
        let mut i = Iface::new("eno1");
        i.prim = true;
        i.h = Some(Box::new([Ring::new(), Ring::new()]));
        i.update(SEC, [1000, 500, 0, 0]);
        i.update(2 * SEC, [3_001_000, 1_000_500, 0, 0]);
        assert_eq!((i.rx, i.tx), (3e6, 1e6));
        assert_eq!(i.part(), (3e6, 1e6));
        i.push(2);
        let h = i.h.as_ref().unwrap();
        assert_eq!((h[0].len(), h[0].back(0), h[1].back(1)), (2, 3e6, 1e6));
        assert!(!i.bad(2 * SEC));
        i.update(3 * SEC, [3_001_000, 1_000_500, 1, 0]);
        assert!(i.bad(3 * SEC) && !i.bad(70 * SEC));
    }

    #[test]
    fn wifi_only_machine() {
        // a laptop: Wi-Fi up, no wired link, /proc/net/wireless has the signal
        assert_eq!(classify("wlp2s0", true, true, "wlan", 1, 0), K_WIFI);
        assert_eq!(classify("wlan0", true, false, "wlan", 1, 0), K_WIFI);
        let w = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
wlp2s0: 0000   58.  -52.  -256        0      0      0      0     17        0
  wlan1: 0000   40.  190.  0        0      0      0      0     17        0
";
        let mut v = Vec::new();
        parse_wireless(w.as_bytes(), &mut |n, s| v.push((n.to_string(), s)));
        assert_eq!(v, vec![("wlp2s0".into(), -52), ("wlan1".into(), -66)]);
        // other kinds
        assert_eq!(classify("eno1", true, false, "", 1, 0), K_WIRED);
        assert_eq!(classify("wg0", false, false, "wireguard", 65534, 0), K_VPN);
        assert_eq!(classify("tailscale0", false, false, "", 65534, 1), K_VPN);
        assert_eq!(classify("tap0", false, false, "", 1, 2), K_VIRT);
        assert_eq!(classify("docker0", false, false, "bridge", 1, 0), K_VIRT);
        assert_eq!(classify("veth1", false, false, "", 1, 0), K_VIRT);
        assert_eq!(classify("lo", false, false, "", 772, 0), K_LO);
        assert_eq!(classify("bond0", false, false, "bond", 1, 0), K_BOND);
    }

    #[test]
    fn default_route() {
        let r =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eno1\t00000000\t0100000A\t0003\t0\t0\t0\t00000000\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
";
        assert_eq!(parse_route(r), vec!["eno1".to_string()]);
        assert!(parse_route("Iface\tDestination\n").is_empty());
    }

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 3500007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   992        0 44901 1 0000000000000000 100 0 0 10 5
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 56676 1 0000000000000000 100 0 0 10 0
   2: 0F00000A:01BD 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4328852 1 0000000000000000 100 0 0 10 0
   3: 0100007F:E6A2 0100007F:1537 01 00000000:00000000 00:00000000 00000000  1000        0 3889 1 0000000000000000 20 4 30 10 -1
   4: 0F00000A:A1B2 0A7100CB:01BB 01 00000000:00000000 02:000A3F2B 00000000  1000        0 3890 2 0000000000000000 20 4 30 10 -1
   5: 0F00000A:A1B3 0A7100CB:01BB 01 00000000:00000000 02:000A3F2B 00000000  1000        0 3891 2 0000000000000000 20 4 30 10 -1
   6: 0F00000A:A1B4 0A7100CB:01BB 06 00000000:00000000 03:00000F35 00000000     0        0 0 3 0000000000000000
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:01BD 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4328856 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 44988 1 0000000000000000 100 0 0 10 0
   2: 0000000000000000FFFF00000F00000A:0016 0000000000000000FFFF00001900000A:FE6E 01 00000000:00000000 02:00011C49 00000000     0        0 77 2 0000000000000000 20 4 1 10 -1
";

    #[test]
    fn proc_tcp_and_tcp6() {
        let mut s = Vec::new();
        parse_proc_socks(TCP, false, false, 1 << ST_LISTEN, &mut s);
        parse_proc_socks(TCP6, true, false, 1 << ST_LISTEN, &mut s);
        assert_eq!(s.len(), 5);
        assert_eq!((s[0].lp, s[0].uid, s[0].ino), (53, 992, 44901));
        assert_eq!(fmt_ip(&s[0].la, false), "127.0.0.53");
        assert_eq!(fmt_ip(&s[3].la, true), "::1");
        let l = group_listen(&s);
        let ports: Vec<(u16, u8, &str)> = l
            .iter()
            .map(|l| (l.port, l.scope, l.addr.as_str()))
            .collect();
        // 22 on every address (v4 and v6), 53 local, 445 on one LAN address (+ ::1)
        assert_eq!(
            ports,
            vec![(22, 0, ""), (53, 1, ""), (445, 2, "10.0.0.15")]
        );
        assert_eq!(l[0].inos, vec![56676, 44988]);
        let mut e = Vec::new();
        parse_proc_socks(TCP, false, false, !(1 << ST_LISTEN), &mut e);
        parse_proc_socks(TCP6, true, false, !(1 << ST_LISTEN), &mut e);
        let p = group_peers(&e, 10);
        assert_eq!(fmt_ip(&p[0].ip, p[0].v6), "203.0.113.10");
        assert_eq!((p[0].port, p[0].n, p[0].local), (443, 2, false));
        assert!(p.iter().any(|p| p.local && p.port == 5431));
        assert!(p.iter().any(|p| fmt_ip(&p.ip, p.v6) == "10.0.0.25"));
        assert_eq!(p.len(), 3); // the TIME_WAIT one isn't a connection
    }

    const API: &str = r#"[{"Id":"459bbc6d13cc727349d26f9b6df499019a0f28c772b0ad48e686cdd384f95f57","Names":["/app-postgres-1"],"Image":"postgres:18","Labels":{"a.b":"{\"Id\":\"x\"}"},"Ports":[{"IP":"127.0.0.1","PrivatePort":5432,"PublicPort":5435,"Type":"tcp"},{"IP":"::1","PrivatePort":5432,"PublicPort":5435,"Type":"tcp"}],"State":"running","Status":"Up 5 hours (healthy)","NetworkSettings":{"Networks":{"app_default":{"IPAddress":"172.18.0.6"}}}},
{"Id":"9a5accf40992c27149ca038ef901cb80343ab9758c2946a42532a34d7cb425e3","Names":["/web"],"Ports":[],"State":"running","Status":"Up 2 minutes (unhealthy)"},
{"Id":"aaaa","Names":["/old"],"State":"exited","Status":"Exited (0) 3 days ago","Ports":[]},
{"Id":"bbbb","Names":["/flappy"],"State":"restarting","Status":"Restarting (1) 5 seconds ago"}]"#;

    #[test]
    fn docker_api_json() {
        let v = parse_api(API.as_bytes());
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].name, "app-postgres-1");
        assert!(v[0].id.starts_with("459bbc6d13cc"));
        assert_eq!((v[0].state, v[0].health), (0, 1));
        assert_eq!(v[0].ports, vec![5435]);
        assert_eq!((v[1].name.as_str(), v[1].health), ("web", 3));
        assert_eq!((v[2].state, v[3].state), (3, 1));
        // the state files' forms
        let c = br#"{"ID":"x","Name":"/db","State":{"Running":true}}"#;
        assert_eq!(jstr(jobjects(c)[0], "Name"), "/db");
        let p = br#"[{"id":"abc","names":["mypod"],"image":"i"}]"#;
        assert_eq!(jstr(jobjects(p)[0], "names"), "mypod");
        assert!(parse_api(b"").is_empty() && parse_api(b"{\"message\":\"x\"}").is_empty());
    }

    #[test]
    fn container_and_service_dirs() {
        let id = "364d335648551ea1c1183517002ef7b200115baa842ceed8bf6c7000571b585d";
        let d = |p: &str, n: &str| ctr_dir(p, n).map(|(k, s)| (k, s.len().min(20), s));
        assert_eq!(
            d("system.slice", &format!("docker-{id}.scope")).unwrap().0,
            C_DOCKER
        );
        assert_eq!(
            d("app.slice", &format!("docker-{id}.scope")).unwrap().2,
            id
        );
        assert_eq!(
            d("user.slice", &format!("libpod-{id}.scope")).unwrap().0,
            C_PODMAN
        );
        assert!(d("user.slice", &format!("libpod-conmon-{id}.scope")).is_none());
        assert_eq!(
            d(
                "kubepods-burstable-pod1.slice",
                &format!("cri-containerd-{id}.scope")
            )
            .unwrap()
            .0,
            C_K8S
        );
        assert_eq!(d("", "lxc.payload.web1").unwrap().2, "web1");
        assert_eq!(d("lxc", "web2").unwrap().0, C_LXC);
        assert_eq!(
            d("machine.slice", "machine-qemu\\x2d1\\x2dubuntu.scope")
                .unwrap()
                .2,
            "qemu-1-ubuntu"
        );
        assert_eq!(d("docker", id).unwrap().0, C_DOCKER); // cgroupfs driver (v1 or v2)
        assert_eq!(d("pod1234", id).unwrap().0, C_K8S);
        assert!(d("system.slice", "docker.service").is_none());
        assert!(d("system.slice", "docker-abc.scope").is_none());
        assert_eq!(
            svc_dir("system.slice", "ssh.service").as_deref(),
            Some("ssh")
        );
        assert_eq!(svc_dir("", "openrc.sshd").as_deref(), Some("sshd")); // OpenRC, no systemd
        assert!(svc_dir("user.slice", "x.service").is_none());
        assert!(descend("system.slice") && descend("user@1000.service") && !descend("ssh.service"));
    }

    /// A cgroup tree in a temp directory: v2 with systemd and Docker, v1 with the
    /// cgroupfs driver, and v2 with neither (no systemd, no containers).
    fn tree(name: &str, files: &[(&str, &str)]) -> String {
        let root = std::env::temp_dir().join(format!("cpuview-net-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (p, body) in files {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        root.to_string_lossy().into_owned()
    }

    fn walked(base: &str) -> Net {
        let mut n = Net::empty();
        (n.cg2, n.cpu_root, n.mem_root) = cg_roots(base);
        n.walk(SEC);
        n.read_ctrs(SEC);
        n.read_ctrs(3 * SEC);
        n
    }

    #[test]
    fn cgroup_v2_layout() {
        let id = "52a5348a903a4c74c1735a1d2c894e462ba203c7348d4987f421ed00c26352f9";
        let sc = format!("system.slice/docker-{id}.scope");
        let base = tree(
            "v2",
            &[
                ("cgroup.controllers", "cpu memory"),
                (
                    &format!("{sc}/cpu.stat"),
                    "usage_usec 1000000\nuser_usec 5\n",
                ),
                (&format!("{sc}/memory.current"), "300000000\n"),
                (
                    &format!("{sc}/memory.stat"),
                    "anon 1\ninactive_file 100000000\n",
                ),
                (&format!("{sc}/cgroup.procs"), ""),
                ("system.slice/ssh.service/cpu.stat", "usage_usec 77\n"),
                ("system.slice/ssh.service/memory.current", "4096\n"),
                ("system.slice/docker.service/cpu.stat", "usage_usec 9\n"),
                ("system.slice/docker.service/memory.current", "8192\n"),
                ("user.slice/cpu.stat", "usage_usec 1\n"),
                ("user.slice/memory.current", "1\n"),
                ("init.scope/cpu.stat", "usage_usec 1\n"),
            ],
        );
        let n = walked(&base);
        assert!(n.cg2);
        assert_eq!(n.ctrs.len(), 1);
        let c = &n.ctrs[0];
        assert_eq!(
            (c.name.as_str(), c.kind, c.mem),
            ("52a5348a903a", C_DOCKER, 2e8)
        );
        let mut s: Vec<&str> = n.svcs.iter().map(|s| s.name.as_str()).collect();
        s.sort();
        assert_eq!(s, vec!["docker", "ssh"]);
        assert_eq!(n.svcs.iter().find(|s| s.name == "ssh").unwrap().mem, 4096.0);
        assert!(n.user.is_some());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn cgroup_v1_layout() {
        let id = "9a5accf40992c27149ca038ef901cb80343ab9758c2946a42532a34d7cb425e3";
        let base = tree(
            "v1",
            &[
                ("cpu,cpuacct/cpuacct.usage", "5\n"),
                (
                    &format!("cpu,cpuacct/docker/{id}/cpuacct.usage"),
                    "2000000000\n",
                ),
                (
                    &format!("memory/docker/{id}/memory.usage_in_bytes"),
                    "50000000\n",
                ),
                (
                    &format!("memory/docker/{id}/memory.stat"),
                    "cache 1\ntotal_inactive_file 10000000\n",
                ),
            ],
        );
        let n = walked(&base);
        assert!(!n.cg2);
        assert_eq!(n.ctrs.len(), 1);
        assert_eq!(n.ctrs[0].mem, 4e7);
        assert!(n.svcs.is_empty() && n.user.is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn no_systemd_no_containers() {
        // Alpine / OpenRC on cgroup v2: services as openrc.* groups, no containers
        let base = tree(
            "openrc",
            &[
                ("cgroup.controllers", "cpu memory"),
                ("openrc.sshd/cpu.stat", "usage_usec 10\n"),
                ("openrc.sshd/memory.current", "1000\n"),
                ("openrc.crond/cpu.stat", "usage_usec 10\n"),
                ("openrc.crond/memory.current", "1000\n"),
            ],
        );
        let n = walked(&base);
        assert!(n.ctrs.is_empty());
        assert_eq!(n.svcs.len(), 2);
        let _ = std::fs::remove_dir_all(&base);
        // runit / no cgroups at all: nothing found, nothing breaks
        let empty = tree("none", &[("x", "")]);
        let n = walked(&empty);
        assert!(n.ctrs.is_empty() && n.svcs.is_empty());
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    #[ignore]
    fn bench_jobs() {
        let mut n = Net::new();
        let now = crate::term::mono();
        n.enter(now);
        for (name, j) in [
            ("listen", J_LISTEN),
            ("peers", J_PEERS),
            ("walk", J_WALK),
            ("api", J_API),
            ("links", J_LINKS),
        ] {
            let c0 = crate::term::cpu_ns();
            for _ in 0..20 {
                n.job(j, now);
            }
            eprintln!("{name}: {} us", (crate::term::cpu_ns() - c0) / 20_000);
        }
        for (name, st) in [
            ("estab v4", 1u32 << ST_ESTAB),
            ("all v4", !0u32),
            ("listen v4", 1 << ST_LISTEN),
        ] {
            let c0 = crate::term::cpu_ns();
            for _ in 0..20 {
                n.socks.clear();
                n.dump(false, false, st);
            }
            eprintln!(
                "{name}: {} us, {} socks",
                (crate::term::cpu_ns() - c0) / 20_000,
                n.socks.len()
            );
        }
        let c0 = crate::term::cpu_ns();
        for _ in 0..20 {
            let _ = std::fs::read_to_string("/proc/net/tcp");
        }
        eprintln!("proc tcp: {} us", (crate::term::cpu_ns() - c0) / 20_000);
    }

    #[test]
    #[ignore]
    fn heap_use() {
        let u = || unsafe { libc::mallinfo2().uordblks / 1024 };
        let a = u();
        let mut n = Net::new();
        let b = u();
        n.enter(crate::term::mono());
        let c = u();
        for j in 0..NJOB {
            let d = u();
            n.job(j, crate::term::mono());
            eprintln!("job {j}: +{} kB", u() as i64 - d as i64);
        }
        eprintln!(
            "new +{} kB, enter +{} kB, now {} kB; cgnames {} owners {} socks cap {} ifs {}",
            b - a,
            c - b,
            u() - a,
            n.cgnames.len(),
            n.owners.len(),
            n.socks.capacity(),
            n.ifs.len()
        );
    }

    #[test]
    fn ipv6_text_like_inet_ntop() {
        let ip = |s: &[u16; 8]| {
            let mut a = [0u8; 16];
            for (k, g) in s.iter().enumerate() {
                a[2 * k..2 * k + 2].copy_from_slice(&g.to_be_bytes());
            }
            fmt_ip(&a, true)
        };
        assert_eq!(ip(&[0, 0, 0, 0, 0, 0, 0, 1]), "::1");
        assert_eq!(ip(&[0; 8]), "::");
        assert_eq!(
            ip(&[0xfe80, 0, 0, 0, 0x1cf8, 0x33ff, 0xfe71, 0x5fba]),
            "fe80::1cf8:33ff:fe71:5fba"
        );
        assert_eq!(ip(&[0x2001, 0xdb8, 0, 0, 1, 0, 0, 1]), "2001:db8::1:0:0:1");
        assert_eq!(
            ip(&[0x2001, 0xdb8, 1, 2, 3, 4, 5, 6]),
            "2001:db8:1:2:3:4:5:6"
        );
        assert_eq!(
            ip(&[0x2001, 0xdb8, 0, 1, 1, 1, 1, 1]),
            "2001:db8:0:1:1:1:1:1"
        );
        assert_eq!(ip(&[1, 0, 0, 0, 0, 0, 0, 0]), "1::");
        assert_eq!(
            ip(&[0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0019]),
            "10.0.0.25"
        );
    }

    #[test]
    fn cgroup_files() {
        assert_eq!(
            kv_val(b"usage_usec 123\nuser_usec 4\n", b"usage_usec"),
            Some(123)
        );
        assert_eq!(kv_val(b"usage_usec_x 1\n", b"usage_usec"), None);
        assert_eq!(cpu_pct((SEC, 1000), 2 * SEC, 501_000), 50.0);
    }
}

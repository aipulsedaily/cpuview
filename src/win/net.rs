//! Network on Windows (the Windows half of net.rs): links and their counters (IP
//! Helper: the interface table, ~0.5–0.9 ms with Windows' dozens of filter and
//! miniport interfaces, so only when the links are looked up; then one GetIfEntry2
//! per link that is up, ~12–75 µs), addresses, connection counts, listening ports and
//! connections with their owning processes. Containers and systemd services are Linux
//! concepts: the page doesn't show them here.

use super::*;
use crate::win::{from_wide, prog_name, ProcList};
use windows_sys::Win32::NetworkManagement::IpHelper::*;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC};

/// MIB_IF_ROW2 flags
const F_HARDWARE: u8 = 1;
const F_FILTER: u8 = 2;
const F_ENDPOINT: u8 = 0x80;
/// IF_TYPE_*
const T_ETHERNET: u32 = 6;
const T_PPP: u32 = 23;
const T_LOOPBACK: u32 = 24;
const T_PROP_VIRTUAL: u32 = 53;
const T_WIFI: u32 = 71;
const T_TUNNEL: u32 = 131;
/// NdisPhysicalMediumBluetooth
const M_BLUETOOTH: i32 = 10;
/// MIB_TCP_STATE_*
const W_LISTEN: u32 = 2;
const W_ESTAB: u32 = 5;
const W_TIME_WAIT: u32 = 11;

/// What the interface table says of a link: kind, or None for Windows' plumbing
/// (filter layers, endpoints, miniports and virtual adapters that are down).
fn kind_of(r: &MIB_IF_ROW2) -> Option<u8> {
    let f = r.InterfaceAndOperStatusFlags._bitfield;
    let up = r.OperStatus == 1;
    if f & (F_FILTER | F_ENDPOINT) != 0 {
        return None;
    }
    let hw = f & F_HARDWARE != 0;
    // WAN miniports (point-to-point "Ethernet" for RAS) and Hyper-V's switch ports
    // carry no traffic of their own
    if !hw && r.Type == T_ETHERNET && (r.AccessType == 3 || from_wide(&r.Description).contains("Virtual Switch Extension")) {
        return None;
    }
    let k = match r.Type {
        T_LOOPBACK => K_LO,
        T_WIFI => K_WIFI,
        T_PPP | T_PROP_VIRTUAL | T_TUNNEL => K_VPN,
        T_ETHERNET if hw => K_WIRED,
        _ if r.TunnelType != 0 => K_VPN,
        _ => K_VIRT,
    };
    // (an unpaired Bluetooth network adapter, a WAN miniport: not links worth listing)
    if !up && (!hw || r.PhysicalMediumType == M_BLUETOOTH) {
        return None;
    }
    Some(k)
}

fn counters(r: &MIB_IF_ROW2) -> [u64; 4] {
    [
        r.InOctets,
        r.OutOctets,
        r.InErrors + r.OutErrors,
        r.InDiscards + r.OutDiscards,
    ]
}

/// Interfaces carrying a default route.
fn default_routes() -> Vec<u32> {
    let mut v = Vec::new();
    let mut t: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIpForwardTable2(AF_UNSPEC, &mut t) } != 0 || t.is_null() {
        return v;
    }
    let rows = unsafe { std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize) };
    for r in rows {
        if r.DestinationPrefix.PrefixLength == 0 && !v.contains(&r.InterfaceIndex) {
            v.push(r.InterfaceIndex);
        }
    }
    unsafe { FreeMibTable(t as *const _) };
    v
}

/// A TCP / UDP table of one family (GetExtended*Table), grown as needed.
fn table(buf: &mut Vec<u8>, v6: bool, udp: bool, class: i32) -> bool {
    let af = if v6 { AF_INET6 } else { AF_INET } as u32;
    buf.resize(buf.len().max(16384), 0);
    for _ in 0..4 {
        let mut size = buf.len() as u32;
        let r = unsafe {
            if udp {
                GetExtendedUdpTable(buf.as_mut_ptr() as *mut _, &mut size, 0, af, class, 0)
            } else {
                GetExtendedTcpTable(buf.as_mut_ptr() as *mut _, &mut size, 0, af, class, 0)
            }
        };
        match r {
            0 => return true,
            // ERROR_INSUFFICIENT_BUFFER: the size it needs now (and room to grow)
            122 => buf.resize(size as usize + size as usize / 4 + 1024, 0),
            _ => return false,
        }
    }
    false
}

/// The rows of a table: (count, rows).
fn rows<T>(buf: &[u8]) -> &[T] {
    let n = u32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize;
    // (rows start after the count, aligned for their type)
    let off = std::mem::align_of::<T>().max(4);
    let max = (buf.len() - off) / std::mem::size_of::<T>();
    unsafe { std::slice::from_raw_parts(buf.as_ptr().add(off) as *const T, n.min(max)) }
}

fn v4(a: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&a.to_ne_bytes());
    b
}

fn port(p: u32) -> u16 {
    u16::from_be(p as u16)
}

impl Net {
    pub fn new() -> Net {
        let mut n = Net::empty();
        n.buf = vec![0; 16384];
        n.links();
        n.read_dev(crate::term::mono());
        n
    }

    /// Counters of the links shown with their own rates (one query each); a link
    /// that went away or changed state has the links looked up again.
    pub(super) fn read_dev(&mut self, now: i64) {
        self.last_dev = now;
        let mut again = false;
        let mut r: MIB_IF_ROW2 = unsafe { std::mem::zeroed() };
        for i in self.ifs.iter_mut().filter(|i| i.prim && i.up) {
            r.InterfaceLuid = unsafe { std::mem::zeroed() };
            r.InterfaceIndex = i.idx;
            if unsafe { GetIfEntry2(&mut r) } != 0 {
                again = true;
                continue;
            }
            i.update(now, counters(&r));
            again |= r.OperStatus != 1;
        }
        if again {
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
    }

    /// (Re)discover links: kinds, state, speed, default route, order (one read of the
    /// interface table).
    pub(super) fn links(&mut self) {
        let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if unsafe { GetIfTable2(&mut t) } != 0 || t.is_null() {
            return;
        }
        let tab = unsafe { std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize) };
        let dflt = default_routes();
        let now = crate::term::mono();
        let mut keep = Vec::new();
        for r in tab {
            let Some(kind) = kind_of(r) else {
                continue;
            };
            keep.push(r.InterfaceIndex);
            let i = match self.ifs.iter().position(|i| i.idx == r.InterfaceIndex) {
                Some(k) => &mut self.ifs[k],
                None => {
                    let mut i = Iface::new(&from_wide(&r.Alias));
                    i.idx = r.InterfaceIndex;
                    i.kind = kind;
                    self.ifs.push(i);
                    self.ifs.last_mut().unwrap()
                }
            };
            i.up = r.OperStatus == 1;
            i.speed = if i.up { (r.ReceiveLinkSpeed / 1_000_000).min(1 << 30) as u32 } else { 0 };
            i.dflt = dflt.contains(&r.InterfaceIndex);
            if i.up && i.cur.0 == 0 {
                i.update(now, counters(r));
            }
        }
        unsafe { FreeMibTable(t as *const _) };
        self.ifs.retain(|i| keep.contains(&i.idx));
        self.ndev = self.ifs.len();
        let any = self.ifs.iter().any(|i| i.kind < K_VIRT);
        let up = self.ifs.iter().any(|i| i.kind < K_VIRT && i.up);
        for i in &mut self.ifs {
            i.prim = i.kind < K_VIRT || (!up && i.kind == K_VIRT && (i.up || !any));
            if i.prim && i.up && i.h.is_none() {
                i.h = Some(Box::new([Ring::new(), Ring::new()]));
            }
            if !i.prim {
                i.h = None;
            }
        }
        let rank = |i: &Iface| {
            (!i.prim as u8) << 3 | (!i.dflt as u8) << 2 | (!i.up as u8) << 1 | (i.kind == K_VPN) as u8
        };
        isort(&mut self.ifs, &|a, b| (rank(a), &a.name) < (rank(b), &b.name));
        self.virt.clear();
        for i in self.ifs.iter().filter(|i| !i.prim && i.kind == K_VIRT && i.up) {
            if !self.virt.is_empty() {
                self.virt += " · ";
            }
            self.virt += &i.name;
        }
    }

    /// Addresses of the links (GetAdaptersAddresses): a global one first (IPv4, then
    /// IPv6), a link-local one only when there is nothing else.
    pub(super) fn addrs(&mut self) {
        for i in &mut self.ifs {
            i.addr_rank = 9;
            i.addr.clear();
        }
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_SKIP_FRIENDLY_NAME;
        let mut buf = std::mem::take(&mut self.buf);
        buf.resize(buf.len().max(16384), 0);
        let mut ok = false;
        for _ in 0..4 {
            let mut size = buf.len() as u32;
            let r = unsafe { GetAdaptersAddresses(AF_UNSPEC as u32, flags, std::ptr::null(), buf.as_mut_ptr() as *mut _, &mut size) };
            if r == 111 {
                // ERROR_BUFFER_OVERFLOW
                buf.resize(size as usize + 4096, 0);
                continue;
            }
            ok = r == 0;
            break;
        }
        let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while ok && !a.is_null() {
            let ad = unsafe { &*a };
            let idx = unsafe { ad.Anonymous1.Anonymous.IfIndex };
            let idx6 = ad.Ipv6IfIndex;
            let mut u = ad.FirstUnicastAddress;
            while !u.is_null() {
                let ua = unsafe { &*u };
                u = ua.Next;
                let sa = ua.Address.lpSockaddr as *const u8;
                if sa.is_null() {
                    continue;
                }
                let fam = unsafe { *(sa as *const u16) };
                let (v6, mut b) = (fam == AF_INET6, [0u8; 16]);
                if fam == AF_INET {
                    b[..4].copy_from_slice(unsafe { std::slice::from_raw_parts(sa.add(4), 4) });
                } else if v6 {
                    b.copy_from_slice(unsafe { std::slice::from_raw_parts(sa.add(8), 16) });
                } else {
                    continue;
                }
                let link = if v6 { b[0] == 0xfe && b[1] & 0xc0 == 0x80 } else { b[0] == 169 && b[1] == 254 };
                if (!v6 && b[0] == 127) || (v6 && b[..15].iter().all(|&x| x == 0)) {
                    continue;
                }
                let rank = link as u8 * 2 + v6 as u8;
                if let Some(i) = self.ifs.iter_mut().find(|i| (i.idx == idx || i.idx == idx6) && i.prim) {
                    if rank < i.addr_rank {
                        i.addr_rank = rank;
                        i.addr = fmt_ip(&b, v6);
                    }
                }
            }
            a = ad.Next;
        }
        buf.truncate(16384);
        self.buf = buf;
    }

    /// Connection counts: open (ESTABLISHED + CLOSE_WAIT) and TCP connections per
    /// family (TCP statistics: constant cost); closing ones (TIME_WAIT) are counted
    /// when the connections are listed.
    pub(super) fn read_counts(&mut self) {
        self.estab = 0;
        for (f, af) in [AF_INET, AF_INET6].into_iter().enumerate() {
            let mut s: MIB_TCPSTATS_LH = unsafe { std::mem::zeroed() };
            if unsafe { GetTcpStatisticsEx(&mut s, af as u32) } == 0 {
                self.estab += s.dwCurrEstab;
                self.inuse[f] = s.dwNumConns;
            }
        }
    }

    /// Listening TCP and bound UDP sockets with their owning processes (the tables
    /// carry the pid, kept in `ino`: owners are named from the process list).
    pub(super) fn read_listen(&mut self) {
        self.socks.clear();
        let mut buf = std::mem::take(&mut self.buf);
        for v6 in [false, true] {
            if table(&mut buf, v6, false, TCP_TABLE_OWNER_PID_LISTENER) {
                if v6 {
                    for r in rows::<MIB_TCP6ROW_OWNER_PID>(&buf) {
                        self.socks.push(Sock { v6, state: ST_LISTEN, la: r.ucLocalAddr, lp: port(r.dwLocalPort), ino: r.dwOwningPid, ..Default::default() });
                    }
                } else {
                    for r in rows::<MIB_TCPROW_OWNER_PID>(&buf) {
                        if r.dwState == W_LISTEN {
                            self.socks.push(Sock { state: ST_LISTEN, la: v4(r.dwLocalAddr), lp: port(r.dwLocalPort), ino: r.dwOwningPid, ..Default::default() });
                        }
                    }
                }
            }
            let n = self.socks.iter().filter(|s| s.v6 == v6).count() as u32;
            self.nlisten[v6 as usize] = n;
        }
        for v6 in [false, true] {
            if table(&mut buf, v6, true, UDP_TABLE_OWNER_PID) {
                if v6 {
                    for r in rows::<MIB_UDP6ROW_OWNER_PID>(&buf) {
                        self.socks.push(Sock { v6, udp: true, state: ST_CLOSE, la: r.ucLocalAddr, lp: port(r.dwLocalPort), ino: r.dwOwningPid, ..Default::default() });
                    }
                } else {
                    for r in rows::<MIB_UDPROW_OWNER_PID>(&buf) {
                        self.socks.push(Sock { udp: true, state: ST_CLOSE, la: v4(r.dwLocalAddr), lp: port(r.dwLocalPort), ino: r.dwOwningPid, ..Default::default() });
                    }
                }
            }
        }
        buf.truncate(16384);
        self.buf = buf;
        self.sock_ok = true;
        self.listen = group_listen(&self.socks);
        // owners: the programs of the pids, from the basic process list
        self.owners.clear();
        let mut pl = ProcList::new();
        if pl.read(false) {
            let socks = &self.socks;
            let owners = &mut self.owners;
            pl.each(&mut |e| {
                if socks.iter().any(|s| s.ino == e.pid) {
                    owners.push((e.pid, prog_name(e.name, e.pid)));
                }
            });
        }
        self.socks = Vec::new();
        self.resolve_owners();
    }

    /// Established connections' remote ends (the connection tables, only for a family
    /// with more connections than listening sockets), and the closing ones' count.
    pub(super) fn read_peers(&mut self) {
        self.socks.clear();
        self.tw = 0;
        let mut buf = std::mem::take(&mut self.buf);
        for (f, v6) in [false, true].into_iter().enumerate() {
            if !(self.inuse[f] > self.nlisten[f] || !self.sock_ok) || !table(&mut buf, v6, false, TCP_TABLE_OWNER_PID_CONNECTIONS) {
                continue;
            }
            let mut add = |st: u32, ra: [u8; 16], rp: u32| {
                if st == W_TIME_WAIT {
                    self.tw += 1;
                } else if st == W_ESTAB {
                    self.socks.push(Sock { v6, state: ST_ESTAB, ra, rp: port(rp), ..Default::default() });
                }
            };
            if v6 {
                for r in rows::<MIB_TCP6ROW_OWNER_PID>(&buf) {
                    add(r.dwState, r.ucRemoteAddr, r.dwRemotePort);
                }
            } else {
                for r in rows::<MIB_TCPROW_OWNER_PID>(&buf) {
                    add(r.dwState, v4(r.dwRemoteAddr), r.dwRemotePort);
                }
            }
        }
        buf.truncate(16384);
        self.buf = buf;
        self.peers = group_peers(&self.socks, 12);
        self.socks = Vec::new();
    }

    /// (sockets are found from the tables directly: no process scan)
    pub(super) fn scan_step(&mut self, _n: usize) {}
}

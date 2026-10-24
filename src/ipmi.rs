//! A server board's BMC (baseboard management controller) through the kernel's IPMI
//! device (/dev/ipmi0): it runs the fans and has its own temperature and voltage
//! sensors, which no hwmon driver shows. Only reads: the sensor list (SDR repository,
//! walked once and cached) and sensor readings.
//!
//! The BMC answers in milliseconds, so readings are asynchronous: each read collects
//! the answers to the previous read's requests and sends new ones; the main loop never
//! waits. Only the sensor-list walk waits (once; later starts use the cache unless the
//! BMC's list changed).
//!
//! /dev/ipmi0 is root-only: whoever opens it can send the BMC any command (power the
//! machine off, change the BMC's users), so `cpuview --setup-sensors --bmc` gives it to
//! the one user who asks, not to everyone.

use std::os::raw::{c_int, c_long, c_ulong};

#[repr(C)]
struct Msg {
    netfn: u8,
    cmd: u8,
    data_len: u16,
    data: *mut u8,
}

#[repr(C)]
struct Req {
    addr: *mut u8,
    addr_len: u32,
    msgid: c_long,
    msg: Msg,
}

#[repr(C)]
struct Recv {
    recv_type: c_int,
    addr: *mut u8,
    addr_len: u32,
    msgid: c_long,
    msg: Msg,
}

/// struct ipmi_system_interface_addr: the BMC itself
#[repr(C)]
struct SysAddr {
    addr_type: c_int,
    channel: i16,
    lun: u8,
}

const fn ioc(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
    (dir << 30) | ((size as c_ulong) << 16) | ((b'i' as c_ulong) << 8) | nr
}
const RECEIVE_MSG_TRUNC: c_ulong = ioc(3, 11, std::mem::size_of::<Recv>());
const SEND_COMMAND: c_ulong = ioc(2, 13, std::mem::size_of::<Req>());
const SYSTEM_INTERFACE_ADDR_TYPE: c_int = 0x0c;
const BMC_CHANNEL: i16 = 0x0f;
const RESPONSE_RECV_TYPE: c_int = 1;

const NETFN_SENSOR: u8 = 0x04;
const NETFN_STORAGE: u8 = 0x0a;
/// msgids of the sensor-list walk (readings use the reading's index)
const WALK: c_long = 1 << 40;

/// Sensor types (IPMI table 42-3) cpuview shows.
pub const T_TEMP: u8 = 0x01;
pub const T_VOLT: u8 = 0x02;
pub const T_FAN: u8 = 0x04;

/// One analog sensor from a full sensor record (SDR type 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Sdr {
    pub num: u8,
    pub lun: u8,
    pub typ: u8,
    /// "FAN1", "CPU Temp", "12V"
    pub name: String,
    /// analog data format: 0 unsigned, 1 one's complement, 2 two's complement
    fmt: u8,
    lin: u8,
    m: i32,
    b: i32,
    rexp: i32,
    bexp: i32,
    /// thresholds the BMC reports: lower critical, lower non-critical, upper
    /// non-critical, upper critical
    pub lc: Option<f32>,
    pub lnc: Option<f32>,
    pub unc: Option<f32>,
    pub uc: Option<f32>,
}

fn sext(v: i32, bits: u32) -> i32 {
    let s = 32 - bits;
    (v << s) >> s
}

impl Sdr {
    /// A full sensor record (header included) of a threshold sensor the BMC owns
    /// that reads temperatures, voltages or fan speeds; None for anything else.
    pub fn parse(r: &[u8]) -> Option<Sdr> {
        if r.len() < 48 || r[3] != 0x01 {
            return None;
        }
        // owned by the BMC (slave address 0x20), a threshold sensor, analog
        let (owner, typ, evt, fmt) = (r[5], r[12], r[13], r[20] >> 6);
        let unit = r[21];
        let ok_unit = match typ {
            T_TEMP => unit == 1,
            T_VOLT => unit == 4,
            T_FAN => unit == 18,
            _ => false,
        };
        if owner != 0x20 || evt != 0x01 || fmt == 3 || !ok_unit || r[22] != 0 {
            return None;
        }
        let n = (r[47] & 0x1f) as usize;
        let id = &r[48..(48 + n).min(r.len())];
        let name = if r[47] >> 6 == 3 {
            String::from_utf8_lossy(id).trim_matches(|c: char| c == '\0' || c == ' ').to_string()
        } else {
            String::new()
        };
        let mut s = Sdr {
            num: r[7],
            lun: r[6] & 3,
            typ,
            name: if name.is_empty() { ["sensor ", &r[7].to_string()].concat() } else { name },
            fmt,
            lin: r[23] & 0x7f,
            m: sext(((r[25] as i32 & 0xc0) << 2) | r[24] as i32, 10),
            b: sext(((r[27] as i32 & 0xc0) << 2) | r[26] as i32, 10),
            rexp: sext(r[29] as i32 >> 4, 4),
            bexp: sext(r[29] as i32 & 0x0f, 4),
            lc: None,
            lnc: None,
            unc: None,
            uc: None,
        };
        // (readable threshold mask: bit 0 lower non-critical, 1 lower critical,
        // 3 upper non-critical, 4 upper critical)
        let m = r[18];
        let th = |bit: u8, at: usize| (m & (1 << bit) != 0).then(|| s.value(r[at]));
        (s.lnc, s.lc, s.unc, s.uc) = (th(0, 41), th(1, 40), th(3, 38), th(4, 37));
        Some(s)
    }

    /// A raw reading in °C, V or RPM: y = L((M·x + B·10^Bexp)·10^Rexp).
    pub fn value(&self, raw: u8) -> f32 {
        let x = match self.fmt {
            0 => raw as f64,
            1 if raw & 0x80 != 0 => -((!raw & 0x7f) as f64),
            1 => raw as f64,
            _ => raw as i8 as f64,
        };
        let y = (self.m as f64 * x + self.b as f64 * 10f64.powi(self.bexp)) * 10f64.powi(self.rexp);
        (match self.lin {
            0 => y,
            1 => y.ln(),
            2 => y.log10(),
            3 => y.log2(),
            4 => y.exp(),
            5 => 10f64.powf(y),
            6 => y.exp2(),
            7 => 1.0 / y,
            8 => y * y,
            9 => y * y * y,
            10 => y.sqrt(),
            11 => y.cbrt(),
            _ => f64::NAN,
        }) as f32
    }
}

/// A Get Sensor Reading answer: the reading, or None if the sensor has none (an
/// empty fan header, scanning off, an error).
pub fn reading(d: &[u8]) -> Option<u8> {
    // completion code, reading, flags (bit 6: scanning on, bit 5: unavailable)
    (d.len() >= 3 && d[0] == 0 && d[2] & 0x40 != 0 && d[2] & 0x20 == 0).then(|| d[1])
}

pub struct Bmc {
    fd: c_int,
}

impl Drop for Bmc {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl Bmc {
    /// The BMC's device, if this user may open it (None: no BMC, or root-only).
    pub fn open(root: &str) -> Option<Bmc> {
        for d in ["/dev/ipmi0", "/dev/ipmi/0", "/dev/ipmidev/0"] {
            let p = std::ffi::CString::new([root, d].concat()).ok()?;
            let fd = unsafe { libc::open(p.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
            if fd >= 0 {
                return Some(Bmc { fd });
            }
        }
        None
    }

    pub fn fd(&self) -> c_int {
        self.fd
    }

    /// Send a request to the BMC (LUN `lun`); false if the driver refused it.
    pub fn send(&self, netfn: u8, cmd: u8, lun: u8, data: &[u8], id: c_long) -> bool {
        let mut a = SysAddr {
            addr_type: SYSTEM_INTERFACE_ADDR_TYPE,
            channel: BMC_CHANNEL,
            lun,
        };
        let mut d = [0u8; 8];
        let n = data.len().min(8);
        d[..n].copy_from_slice(&data[..n]);
        let mut r = Req {
            addr: &mut a as *mut SysAddr as *mut u8,
            addr_len: std::mem::size_of::<SysAddr>() as u32,
            msgid: id,
            msg: Msg {
                netfn,
                cmd,
                data_len: n as u16,
                data: d.as_mut_ptr(),
            },
        };
        unsafe { libc::ioctl(self.fd, SEND_COMMAND as _, &mut r) == 0 }
    }

    /// The next answer that has arrived: its msgid and data (completion code first).
    pub fn recv<'a>(&self, buf: &'a mut [u8; 288]) -> Option<(c_long, &'a [u8])> {
        loop {
            let mut addr = [0u8; 40];
            let mut r = Recv {
                recv_type: 0,
                addr: addr.as_mut_ptr(),
                addr_len: addr.len() as u32,
                msgid: 0,
                msg: Msg {
                    netfn: 0,
                    cmd: 0,
                    data_len: buf.len() as u16,
                    data: buf.as_mut_ptr(),
                },
            };
            if unsafe { libc::ioctl(self.fd, RECEIVE_MSG_TRUNC as _, &mut r) } != 0 {
                // (EMSGSIZE: truncated, still delivered)
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EMSGSIZE) {
                    return None;
                }
            }
            if r.recv_type == RESPONSE_RECV_TYPE {
                let n = (r.msg.data_len as usize).min(buf.len());
                return Some((r.msgid, &buf[..n]));
            }
        }
    }

    /// One request and its answer, waiting up to `ms` (the sensor-list walk only).
    fn call(&self, netfn: u8, cmd: u8, data: &[u8], ms: c_int, out: &mut Vec<u8>) -> Option<u8> {
        static SEQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
        let id = WALK + SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as c_long;
        if !self.send(netfn, cmd, 0, data, id) {
            return None;
        }
        let mut buf = [0u8; 288];
        let end = crate::term::now_ns(libc::CLOCK_MONOTONIC) + ms as i64 * 1_000_000;
        loop {
            let left = ((end - crate::term::now_ns(libc::CLOCK_MONOTONIC)) / 1_000_000) as c_int;
            let mut p = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if left <= 0 || unsafe { libc::poll(&mut p, 1, left) } <= 0 {
                return None;
            }
            while let Some((m, d)) = self.recv(&mut buf) {
                if m == id && !d.is_empty() {
                    out.clear();
                    out.extend_from_slice(&d[1..]);
                    return Some(d[0]);
                }
            }
        }
    }

    /// The SDR repository's identity (version, record count, last addition and
    /// erase times): a cached list is good while this matches.
    fn repo_key(&self) -> Option<Vec<u8>> {
        let mut d = Vec::new();
        (self.call(NETFN_STORAGE, 0x20, &[], 1000, &mut d)? == 0 && d.len() >= 13).then(|| d[..13].to_vec())
    }

    /// Every record of the SDR repository (each with its 5-byte header), at most
    /// `budget_ms` of waiting.
    fn walk(&self, budget_ms: i64) -> Option<Vec<Vec<u8>>> {
        let t0 = crate::term::now_ns(libc::CLOCK_MONOTONIC);
        let late = || crate::term::now_ns(libc::CLOCK_MONOTONIC) - t0 > budget_ms * 1_000_000;
        let mut d = Vec::new();
        // (a reservation: partial reads need one; 0 if the BMC doesn't do them)
        let reserve = |d: &mut Vec<u8>| -> Option<[u8; 2]> {
            let cc = self.call(NETFN_STORAGE, 0x22, &[], 1000, d)?;
            Some(if cc == 0 && d.len() >= 2 { [d[0], d[1]] } else { [0, 0] })
        };
        let mut resv = reserve(&mut d)?;
        let mut recs = Vec::new();
        let mut id = [0u8, 0u8];
        let mut retry = 0;
        while id != [0xff, 0xff] && recs.len() < 1024 {
            if late() {
                return None;
            }
            // the whole record at once; BMCs that can't return that many bytes get
            // it in pieces (header first, then 16 bytes at a time)
            let cc = self.call(NETFN_STORAGE, 0x23, &[resv[0], resv[1], id[0], id[1], 0, 0xff], 1000, &mut d)?;
            if cc == 0xc5 && retry < 3 {
                // (reservation cancelled: the list changed under us)
                retry += 1;
                resv = reserve(&mut d)?;
                continue;
            }
            let (next, rec) = if cc == 0 && d.len() >= 7 {
                ([d[0], d[1]], d[2..].to_vec())
            } else {
                if self.call(NETFN_STORAGE, 0x23, &[resv[0], resv[1], id[0], id[1], 0, 5], 1000, &mut d)? != 0 || d.len() < 7 {
                    return None;
                }
                let next = [d[0], d[1]];
                let mut rec = d[2..7].to_vec();
                let len = rec[4] as usize + 5;
                while rec.len() < len {
                    let k = (len - rec.len()).min(16) as u8;
                    if self.call(NETFN_STORAGE, 0x23, &[resv[0], resv[1], id[0], id[1], rec.len() as u8, k], 1000, &mut d)? != 0 || d.len() < 3 {
                        return None;
                    }
                    rec.extend_from_slice(&d[2..]);
                }
                (next, rec)
            };
            recs.push(rec);
            if next == id {
                break;
            }
            id = next;
        }
        Some(recs)
    }

    /// The analog temperature, voltage and fan sensors, from the cache file if the
    /// BMC's list hasn't changed since, else walked (and cached).
    pub fn sensors(&self, cache: &std::path::Path) -> Vec<Sdr> {
        let Some(key) = self.repo_key() else { return Vec::new() };
        let recs = match std::fs::read(cache) {
            Ok(b) if b.len() > 13 && b[..13] == key[..] => split(&b[13..]),
            _ => {
                let Some(recs) = self.walk(5000) else { return Vec::new() };
                let mut b = key.clone();
                for r in &recs {
                    b.extend_from_slice(r);
                }
                if let Some(d) = cache.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(cache, b);
                recs
            }
        };
        recs.iter().filter_map(|r| Sdr::parse(r)).collect()
    }

    /// Ask for sensor `s`'s reading (answered by a later `recv`, msgid `id`).
    pub fn ask(&self, s: &Sdr, id: usize) -> bool {
        self.send(NETFN_SENSOR, 0x2d, s.lun, &[s.num], id as c_long)
    }
}

/// Records laid end to end (each 5-byte header gives its length).
pub fn split(mut b: &[u8]) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    while b.len() >= 5 {
        let n = (b[4] as usize + 5).min(b.len());
        v.push(b[..n].to_vec());
        b = &b[n..];
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full sensor record like a Supermicro X12's.
    pub fn full(num: u8, typ: u8, unit: u8, m: u8, rexp_bexp: u8, mask: u8, th: [u8; 6], name: &str) -> Vec<u8> {
        let mut r = vec![0u8; 48];
        r[2] = 0x51;
        r[3] = 0x01;
        r[5] = 0x20;
        r[7] = num;
        r[12] = typ;
        r[13] = 0x01;
        r[18] = mask;
        r[21] = unit;
        r[24] = m;
        r[29] = rexp_bexp;
        // unr, uc, unc, lnr, lc, lnc
        r[36..42].copy_from_slice(&th);
        r[47] = 0xc0 | name.len() as u8;
        r.extend_from_slice(name.as_bytes());
        r[4] = (r.len() - 5) as u8;
        r
    }

    #[test]
    fn struct_sizes_match_the_kernel() {
        // (64-bit: struct ipmi_req 40 bytes, struct ipmi_recv 48)
        if std::mem::size_of::<usize>() == 8 {
            assert_eq!(SEND_COMMAND, 0x8028_690d);
            assert_eq!(RECEIVE_MSG_TRUNC, 0xc030_690b);
        }
    }

    #[test]
    fn fan_temperature_and_voltage_records() {
        // fans: M = 100, RPM; lower thresholds readable
        let f = Sdr::parse(&full(0x41, T_FAN, 18, 100, 0x00, 0x07, [0xff, 0xff, 0xff, 2, 4, 5], "FAN1")).unwrap();
        assert_eq!((f.name.as_str(), f.num, f.typ), ("FAN1", 0x41, T_FAN));
        assert_eq!(f.value(43), 4300.0);
        assert_eq!(f.value(138), 13800.0);
        assert_eq!((f.lc, f.lnc, f.unc, f.uc), (Some(400.0), Some(500.0), None, None));
        // temperatures: two's complement, M = 1
        let mut t = full(0x01, T_TEMP, 1, 1, 0x00, 0x18, [100, 90, 85, 0, 0, 0], "CPU Temp");
        t[20] = 0x80;
        let t = Sdr::parse(&t).unwrap();
        assert_eq!(t.value(30), 30.0);
        assert_eq!(t.value(0xfb), -5.0);
        assert_eq!((t.unc, t.uc), (Some(85.0), Some(90.0)));
        // voltages: M = 59, Rexp −3 (12 V rail: 203 · 0.059 = 11.977 V)
        let v = Sdr::parse(&full(0x30, T_VOLT, 4, 59, 0xd0, 0, [0; 6], "12V")).unwrap();
        assert!((v.value(203) - 11.977).abs() < 1e-3, "{}", v.value(203));
        // not shown: discrete sensors, other owners, other units, compact records
        let mut d = full(0x50, T_FAN, 18, 100, 0, 0, [0; 6], "x");
        d[13] = 0x6f;
        assert!(Sdr::parse(&d).is_none());
        let mut o = full(0x51, T_TEMP, 1, 1, 0, 0, [0; 6], "x");
        o[5] = 0x2c;
        assert!(Sdr::parse(&o).is_none());
        assert!(Sdr::parse(&full(0x52, T_TEMP, 4, 1, 0, 0, [0; 6], "x")).is_none());
        let mut c = full(0x53, T_TEMP, 1, 1, 0, 0, [0; 6], "x");
        c[3] = 0x02;
        assert!(Sdr::parse(&c).is_none());
    }

    #[test]
    fn readings_and_empty_headers() {
        assert_eq!(reading(&[0, 43, 0xc0]), Some(43));
        // ipmitool's "No Reading": unavailable, or scanning off
        assert_eq!(reading(&[0, 0, 0xe0]), None);
        assert_eq!(reading(&[0, 0, 0x00]), None);
        assert_eq!(reading(&[0xcb]), None);
    }

    #[test]
    fn cached_records_split_back() {
        let a = full(1, T_FAN, 18, 100, 0, 0, [0; 6], "FAN1");
        let b = full(2, T_TEMP, 1, 1, 0, 0, [0; 6], "CPU Temp");
        let v = split(&[a.clone(), b.clone()].concat());
        assert_eq!(v, vec![a, b]);
    }
}

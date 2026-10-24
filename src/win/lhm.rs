//! LibreHardwareMonitor's readings (CPU, GPU and board temperatures, fans, voltages,
//! CPU package power): Windows has no interface for these without a kernel driver,
//! which LibreHardwareMonitor brings. Its web server (Options → Remote Web Server,
//! port 8085; CPUVIEW_LHM=host:port for another) serves them as data.json. A helper
//! thread fetches it when asked (rarely: the parse costs a few hundred µs), so the
//! main loop only copies the last snapshot.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering::Relaxed};
use std::sync::Mutex;
use windows_sys::Win32::System::Threading::{WaitOnAddress, WakeByAddressSingle, INFINITE};

/// One reading: LHM's sensor id ("/intelcpu/0/temperature/0"), its hardware's id
/// ("/intelcpu/0") and name, the sensor's name ("CPU Package"), type, value (°C, RPM,
/// V, W; NaN when it has none).
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub id: String,
    pub hwid: String,
    pub hw: String,
    pub text: String,
    pub typ: String,
    pub val: f32,
}

static SNAP: Mutex<Vec<Reading>> = Mutex::new(Vec::new());
/// fetches done (a new snapshot), asked for (futex-style word), thread started
static GEN: AtomicU32 = AtomicU32::new(0);
static ASK: AtomicU32 = AtomicU32::new(0);
static UP: AtomicBool = AtomicBool::new(false);
/// CPU time (ns) of the helper's last fetch
pub static COST: AtomicI64 = AtomicI64::new(0);

/// host:port of the web server.
fn addr() -> String {
    std::env::var("CPUVIEW_LHM").unwrap_or_else(|_| "127.0.0.1:8085".into())
}

/// Something listens on the web server's port of this machine (the TCP listener
/// table: no connection attempt, which Windows retries for ~2 s when refused).
pub fn listening() -> bool {
    let a = addr();
    let Some(port) = a.rsplit_once(':').and_then(|p| p.1.parse::<u16>().ok()) else {
        return false;
    };
    if !a.starts_with("127.") && !a.starts_with("localhost") {
        return true;
    }
    let mut buf = vec![0u8; 65536];
    for v6 in [false, true] {
        let af = if v6 { 23 } else { 2 };
        for _ in 0..3 {
            let mut size = buf.len() as u32;
            // TCP_TABLE_BASIC_LISTENER (v4) / OWNER_PID_LISTENER (v6: basic tables are
            // v4 only)
            let r = unsafe {
                windows_sys::Win32::NetworkManagement::IpHelper::GetExtendedTcpTable(
                    buf.as_mut_ptr() as *mut _,
                    &mut size,
                    0,
                    af,
                    if v6 { 3 } else { 0 },
                    0,
                )
            };
            if r == 122 {
                buf.resize(size as usize + 4096, 0);
                continue;
            }
            if r == 0 && listening_in(&buf, v6, port) {
                return true;
            }
            break;
        }
    }
    false
}

fn listening_in(b: &[u8], v6: bool, port: u16) -> bool {
    let n = u32::from_ne_bytes(b[..4].try_into().unwrap()) as usize;
    // rows: v4 basic 5 u32 (state, local addr, local port, …), v6 owner-pid 14 u32
    // (local addr 16 bytes, scope, local port at word 5)
    let (w, at) = if v6 { (14, 5) } else { (5, 2) };
    (0..n).any(|k| {
        let o = 4 + 4 * (k * w + at);
        o + 4 <= b.len() && u16::from_be(u32::from_ne_bytes(b[o..o + 4].try_into().unwrap()) as u16) == port
    })
}

/// Fetch and parse data.json now (None: no server, or not LibreHardwareMonitor's).
pub fn fetch() -> Option<Vec<Reading>> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let a = addr();
    let sa = a.to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    s.write_all(b"GET /data.json HTTP/1.0\r\nHost: localhost\r\nAccept: application/json\r\n\r\n").ok()?;
    let mut b = Vec::with_capacity(64 * 1024);
    s.take(8 << 20).read_to_end(&mut b).ok()?;
    let h = b.windows(4).position(|w| w == b"\r\n\r\n")?;
    if !b.starts_with(b"HTTP/1.") || b.get(9..12) != Some(b"200") {
        return None;
    }
    let mut out = Vec::new();
    parse(&b[h + 4..], &mut out)?;
    Some(out)
}

/// The readings of an LHM data.json document.
pub fn parse(b: &[u8], out: &mut Vec<Reading>) -> Option<()> {
    let mut p = P { b, i: 0 };
    let v = p.value(0)?;
    walk(&v, "", "", out);
    Some(())
}

/// Every sensor node under `v`, with the hardware node it belongs to.
fn walk(v: &J, hwid: &str, hw: &str, out: &mut Vec<Reading>) {
    let J::Obj(f) = v else { return };
    let get = |k: &str| f.iter().find(|x| x.0 == k).map(|x| &x.1);
    let s = |k: &str| match get(k) {
        Some(J::Str(s)) => s.as_str(),
        _ => "",
    };
    let text = s("Text");
    let (mut hwid, mut hw) = (hwid, hw);
    if let Some(J::Str(id)) = get("HardwareId") {
        (hwid, hw) = (id.as_str(), text);
    }
    // (sensor nodes carry their id and type; LibreHardwareMonitor 0.9 and later)
    let typ = s("Type");
    if !typ.is_empty() && get("SensorId").is_some() {
        let val = match get("RawValue") {
            Some(J::Num(n)) => *n as f32,
            _ => value_text(s("Value")),
        };
        out.push(Reading {
            id: s("SensorId").to_string(),
            hwid: hwid.to_string(),
            hw: hw.to_string(),
            text: text.to_string(),
            typ: typ.to_string(),
            val,
        });
    }
    if let Some(J::Arr(c)) = get("Children") {
        for x in c {
            walk(x, hwid, hw, out);
        }
    }
}

/// "45.0 °C", "1,250 V" (the server's locale) → the number.
fn value_text(s: &str) -> f32 {
    let n: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-'))
        .map(|c| if c == ',' { '.' } else { c })
        .collect();
    crate::sys::pf(&n).unwrap_or(f32::NAN)
}

/// A JSON value (the subset data.json uses).
#[derive(Debug)]
enum J {
    Null,
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn value(&mut self, depth: u32) -> Option<J> {
        if depth > 64 {
            return None;
        }
        self.ws();
        let c = *self.b.get(self.i)?;
        match c {
            b'{' => {
                self.i += 1;
                let mut v = Vec::new();
                loop {
                    self.ws();
                    if self.b.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        return Some(J::Obj(v));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return None;
                    }
                    self.i += 1;
                    let x = self.value(depth + 1)?;
                    v.push((k, x));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                loop {
                    self.ws();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Some(J::Arr(v));
                    }
                    v.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {}
                        _ => return None,
                    }
                }
            }
            b'"' => self.string().map(J::Str),
            b'n' => {
                self.i += 4;
                Some(J::Null)
            }
            b't' | b'f' => {
                self.i += if c == b't' { 4 } else { 5 };
                Some(J::Null)
            }
            _ => {
                let s = self.i;
                while self.i < self.b.len() && matches!(self.b[self.i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[s..self.i]).ok()?;
                // ("1.2E+03": the shared parser takes no '+')
                crate::sensors::parse_sci(&t.replace('+', "")).map(J::Num)
            }
        }
    }
    fn string(&mut self) -> Option<String> {
        if self.b.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out: Vec<u16> = Vec::new();
        let mut raw = Vec::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    if !raw.is_empty() {
                        out.extend(String::from_utf8_lossy(&std::mem::take(&mut raw)).encode_utf16());
                    }
                    match e {
                        b'u' => {
                            let h = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
                            out.push(u16::from_str_radix(h, 16).ok()?);
                            self.i += 4;
                        }
                        b'n' => out.push(10),
                        b't' => out.push(9),
                        b'r' => out.push(13),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        _ => out.push(e as u16),
                    }
                }
                _ => raw.push(c),
            }
        }
        if !raw.is_empty() {
            out.extend(String::from_utf8_lossy(&raw).encode_utf16());
        }
        Some(String::from_utf16_lossy(&out))
    }
}

fn main_loop() {
    loop {
        let a = ASK.load(Relaxed);
        let t0 = crate::term::now_ns(crate::term::CLOCK_THREAD_CPUTIME_ID);
        if let Some(v) = fetch() {
            *SNAP.lock().unwrap_or_else(|e| e.into_inner()) = v;
        }
        COST.store(crate::term::now_ns(crate::term::CLOCK_THREAD_CPUTIME_ID) - t0, Relaxed);
        GEN.fetch_add(1, Relaxed);
        // until the next request
        while ASK.load(Relaxed) == a {
            unsafe { WaitOnAddress(ASK.as_ptr() as *const _, &a as *const u32 as *const _, 4, INFINITE) };
        }
    }
}

/// Ask the helper for a fresh snapshot (started on first use).
pub fn request() {
    ASK.fetch_add(1, Relaxed);
    if !UP.swap(true, Relaxed) {
        let _ = std::thread::Builder::new().stack_size(256 * 1024).spawn(main_loop);
    } else {
        unsafe { WakeByAddressSingle(ASK.as_ptr() as *const _) };
    }
}

/// The last snapshot (set directly by a synchronous fetch at detection).
pub fn set(v: Vec<Reading>) {
    *SNAP.lock().unwrap_or_else(|e| e.into_inner()) = v;
}

/// Look at the last snapshot.
pub fn with<T>(f: impl FnOnce(&[Reading]) -> T) -> T {
    f(&SNAP.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Snapshots fetched so far by the helper.
pub fn gen() -> u32 {
    GEN.load(Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_json() {
        let j = r#"{"id":0,"Version":"0.9.4","Text":"Sensor","Min":"Min","Value":"Value","Max":"Max","ImageURL":"","Children":[{"id":1,"Text":"PC","Min":"","Value":"","Max":"","ImageURL":"images_icon/computer.png","Children":[
{"id":2,"Text":"Intel Core i9-13900K","Min":"","Value":"","Max":"","HardwareId":"/intelcpu/0","ImageURL":"images_icon/cpu.png","Children":[
 {"id":3,"Text":"Temperatures","Min":"","Value":"","Max":"","ImageURL":"images_icon/temperature.png","Children":[
  {"id":4,"Text":"CPU Package","Min":"30.0 °C","Value":"45.0 °C","Max":"80.0 °C","SensorId":"/intelcpu/0/temperature/27","Type":"Temperature","RawMin":30,"RawValue":45.5,"RawMax":80,"ImageURL":"images/transparent.png","Children":[]}]},
 {"id":5,"Text":"Powers","ImageURL":"images_icon/power.png","Children":[
  {"id":6,"Text":"CPU Package","Value":"35,2 W","SensorId":"/intelcpu/0/power/0","Type":"Power","RawValue":null,"Children":[]}]}]},
{"id":7,"Text":"Nuvoton NCT6798D","HardwareId":"/lpc/nct6798d/0","ImageURL":"images_icon/chip.png","Children":[
 {"id":8,"Text":"Fans","ImageURL":"images_icon/fan.png","Children":[
  {"id":9,"Text":"Fan #2","Value":"1200 RPM","SensorId":"/lpc/nct6798d/0/fan/1","Type":"Fan","RawValue":1.2E+03,"Children":[]}]}]}]}]}"#;
        let mut v = Vec::new();
        parse(j.as_bytes(), &mut v).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].text.as_str(), v[0].hw.as_str(), v[0].hwid.as_str(), v[0].typ.as_str()), ("CPU Package", "Intel Core i9-13900K", "/intelcpu/0", "Temperature"));
        assert_eq!(v[0].val, 45.5);
        assert_eq!(v[1].val, 35.2);
        assert_eq!((v[2].hwid.as_str(), v[2].val), ("/lpc/nct6798d/0", 1200.0));
        assert_eq!(value_text("1,250 V"), 1.25);
        assert_eq!(value_text("45.0 °C"), 45.0);
    }
}

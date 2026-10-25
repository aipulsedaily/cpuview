//! cpuview's sensor service: CPU temperatures that need a driver (PawnIO, see
//! win/pawnio.rs and win/cputemp.rs), read by a small service that runs as SYSTEM
//! (the driver opens for SYSTEM and administrators only) and published read-only for
//! cpuview, which runs as the user.
//!
//! The service idles until a cpuview asks (an event any signed-in user may set),
//! reads the sensors then (at most every 0.5 s) and writes them into a shared
//! snapshot users may only read; it takes no other input. cpuview's side costs a
//! few µs: setting the event and copying the snapshot. Installed and removed by
//! `cpuview --setup-sensors` (win/sensors.rs), which copies cpuview.exe and the
//! modules to where only administrators may write.

use super::cputemp::{Cpu, Reading};
use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{LocalFree, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{CreateEventW, OpenEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE};

pub const SERVICE: &str = "cpuview-sensors";
/// the snapshot's size, its header, an entry, entries at most
const SIZE: usize = 4096;
const HEAD: usize = 24;
const ENTRY: usize = 32;
const MAX: usize = (SIZE - HEAD) / ENTRY;
const MAGIC: u32 = u32::from_le_bytes(*b"CPVS");
const VERSION: u32 = 1;
/// header status: the service runs but has nothing to read (driver or module missing,
/// or a CPU it doesn't know)
pub const ST_NONE: u32 = 1;
/// entry flags
const F_MINOR: u8 = 1;

/// The snapshot's and the event's names: global (the service's session isn't the
/// user's), or (CPUVIEW_SENSORS_NS=Local) in the session, to try the service out
/// without installing it.
fn name(what: &str) -> Vec<u16> {
    let ns = if std::env::var("CPUVIEW_SENSORS_NS").is_ok_and(|v| v == "Local") { "Local" } else { "Global" };
    wide(&[ns, "\\cpuview-sensors", what].concat())
}

/// A snapshot in memory: header (magic, version, sequence: odd while being written,
/// count, status, the service's clock in ms at the read) and entries (°C, flags,
/// label length, label).
fn write(p: *mut u8, rd: &[Reading], status: u32) {
    let seq = unsafe { &*(p.add(8) as *const AtomicU32) };
    let s = seq.load(Ordering::Relaxed);
    seq.store(s | 1, Ordering::Relaxed);
    std::sync::atomic::fence(Ordering::Release);
    let n = rd.len().min(MAX);
    unsafe {
        for (k, r) in rd.iter().take(n).enumerate() {
            let e = p.add(HEAD + k * ENTRY);
            let l = r.label.as_bytes();
            let ln = l.len().min(ENTRY - 7);
            std::ptr::write_unaligned(e as *mut f32, r.val);
            *e.add(4) = if r.minor { F_MINOR } else { 0 };
            *e.add(5) = 1;
            *e.add(6) = ln as u8;
            std::ptr::copy_nonoverlapping(l.as_ptr(), e.add(7), ln);
        }
        std::ptr::write_unaligned(p.add(12) as *mut u32, n as u32);
        std::ptr::write_unaligned(p.add(16) as *mut u32, status);
        std::ptr::write_unaligned(p.add(20) as *mut u32, GetTickCount64() as u32);
        std::ptr::write_unaligned(p as *mut u32, MAGIC);
        std::ptr::write_unaligned(p.add(4) as *mut u32, VERSION);
    }
    std::sync::atomic::fence(Ordering::Release);
    seq.store((s | 1) + 1, Ordering::Release);
}

/// A consistent copy of a snapshot (None: not one, or always being written): its
/// sequence number, status and readings.
fn read(p: *const u8) -> Option<(u32, u32, Vec<Reading>)> {
    let seq = unsafe { &*(p.add(8) as *const AtomicU32) };
    for _ in 0..4 {
        let s = seq.load(Ordering::Acquire);
        if s & 1 != 0 {
            std::thread::yield_now();
            continue;
        }
        let (magic, ver, n, st) = unsafe {
            (
                std::ptr::read_unaligned(p as *const u32),
                std::ptr::read_unaligned(p.add(4) as *const u32),
                std::ptr::read_unaligned(p.add(12) as *const u32) as usize,
                std::ptr::read_unaligned(p.add(16) as *const u32),
            )
        };
        if magic != MAGIC || ver != VERSION {
            return None;
        }
        let mut out = Vec::with_capacity(n.min(MAX));
        for k in 0..n.min(MAX) {
            let e = unsafe { p.add(HEAD + k * ENTRY) };
            let (val, fl, ln) = unsafe { (std::ptr::read_unaligned(e as *const f32), *e.add(4), (*e.add(6) as usize).min(ENTRY - 7)) };
            let label = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(e.add(7), ln) }).into_owned();
            out.push(Reading { label, val, minor: fl & F_MINOR != 0 });
        }
        std::sync::atomic::fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) == s {
            return Some((s, st, out));
        }
    }
    None
}

/// A security descriptor from SDDL (LocalFree it).
fn sd(sddl: &str) -> *mut core::ffi::c_void {
    let mut p = std::ptr::null_mut();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), 1, &mut p, std::ptr::null_mut()) };
    p
}

// ── the service ────────────────────────────────────────────────────────────

static STATUS: Mutex<usize> = Mutex::new(0);
static STOP: Mutex<usize> = Mutex::new(0);

/// `cpuview --sensor-service`: run as the service (started by the service manager),
/// or with `--console` in this console (to try it: CPUVIEW_SENSORS_NS=Local).
pub fn main() -> i32 {
    let mut name = wide(SERVICE);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } != 0 {
        return 0;
    }
    if std::env::args().any(|a| a == "--console") {
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        println!("cpuview sensor service: serving (Ctrl-C ends)");
        return serve(stop);
    }
    eprintln!("cpuview: --sensor-service is run by Windows' service manager (cpuview --setup-sensors installs it)");
    2
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let h = unsafe { RegisterServiceCtrlHandlerExW(wide(SERVICE).as_ptr(), Some(handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = h as usize;
    let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    *STOP.lock().unwrap_or_else(|e| e.into_inner()) = stop as usize;
    set_status(SERVICE_RUNNING);
    let code = serve(stop);
    set_status_code(SERVICE_STOPPED, code as u32);
}

unsafe extern "system" fn handler(control: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_status(SERVICE_STOP_PENDING);
            let stop = *STOP.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { SetEvent(stop as HANDLE) };
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

fn set_status(state: SERVICE_STATUS_CURRENT_STATE) {
    set_status_code(state, 0);
}

fn set_status_code(state: SERVICE_STATUS_CURRENT_STATE, code: u32) {
    let h = *STATUS.lock().unwrap_or_else(|e| e.into_inner());
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: if code != 0 { 1066 } else { 0 }, // ERROR_SERVICE_SPECIFIC_ERROR
        dwServiceSpecificExitCode: code,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_STOP_PENDING { 2000 } else { 0 },
    };
    if h != 0 {
        unsafe { SetServiceStatus(h as SERVICE_STATUS_HANDLE, &st) };
    }
}

/// Serve the CPU's temperatures until `stop` is set. The modules come from the
/// `pawnio` folder next to this program (where the installer put them: only
/// administrators may write there); without a reader (no driver yet, a CPU it
/// doesn't know) it tries again a minute later when asked.
fn serve(stop: HANDLE) -> i32 {
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("pawnio"))).unwrap_or_default();
    let mut cpu = Cpu::open(&dir);
    let mut tried = unsafe { GetTickCount64() };
    serve_with(stop, &mut || {
        let now = unsafe { GetTickCount64() };
        if cpu.is_none() && now >= tried + 60_000 {
            tried = now;
            cpu = Cpu::open(&dir);
        }
        cpu.as_ref().map(|c| c.read())
    })
}

/// Serve until `stop` is set: the snapshot (SYSTEM may write, signed-in users read)
/// and the event (users may set it), then `read` whenever asked, 0.5 s apart at
/// most (None: nothing to read here).
fn serve_with(stop: HANDLE, read: &mut dyn FnMut() -> Option<Vec<Reading>>) -> i32 {
    let (map_sd, ev_sd) = (sd("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;AU)"), sd("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x100002;;;AU)"));
    let sa = |p| SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: p, bInheritHandle: 0 };
    let (msa, esa) = (sa(map_sd), sa(ev_sd));
    let map = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, &msa, PAGE_READWRITE, 0, SIZE as u32, name("").as_ptr()) };
    let ask = unsafe { CreateEventW(&esa, 0, 0, name("-ask").as_ptr()) };
    unsafe {
        LocalFree(map_sd as _);
        LocalFree(ev_sd as _);
    }
    if map.is_null() || ask.is_null() {
        return 3;
    }
    let view = unsafe { MapViewOfFile(map, FILE_MAP_WRITE, 0, 0, SIZE) }.Value as *mut u8;
    if view.is_null() {
        close(ask);
        close(map);
        return 4;
    }
    write(view, &[], 0);
    let mut last = 0u64;
    loop {
        let w = unsafe { WaitForMultipleObjects(2, [stop, ask].as_ptr(), 0, INFINITE) };
        if w != WAIT_OBJECT_0 + 1 {
            break;
        }
        // (0.5 s apart at most: the rest of the wait, unless stopped)
        let now = unsafe { GetTickCount64() };
        if now < last + 500 && unsafe { WaitForSingleObject(stop, (last + 500 - now) as u32) } == WAIT_OBJECT_0 {
            break;
        }
        last = unsafe { GetTickCount64() };
        match read() {
            Some(v) => write(view, &v, 0),
            None => write(view, &[], ST_NONE),
        }
    }
    unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
    close(ask);
    close(map);
    0
}

// ── cpuview's side ─────────────────────────────────────────────────────────

/// The service's snapshot, mapped read-only, its event, the last sequence seen, and
/// since when (ms) it hasn't moved although asked.
struct Client {
    map: HANDLE,
    view: *const u8,
    ask: HANDLE,
    seq: u32,
    stuck: u64,
}

// (the handles and the read-only view are only used under the mutex below)
unsafe impl Send for Client {}

impl Client {
    fn open() -> Option<Client> {
        let map = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name("").as_ptr()) };
        if map.is_null() {
            return None;
        }
        let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, SIZE) }.Value as *const u8;
        // (EVENT_MODIFY_STATE)
        let ask = unsafe { OpenEventW(0x0002, 0, name("-ask").as_ptr()) };
        if view.is_null() || ask.is_null() {
            close(ask);
            close(map);
            return None;
        }
        Some(Client { map, view, ask, seq: u32::MAX, stuck: 0 })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut _ }) };
        close(self.ask);
        close(self.map);
    }
}

static CLIENT: Mutex<Option<Client>> = Mutex::new(None);
static LOOKED: Mutex<u64> = Mutex::new(0);

/// The service's last readings (None: no service, or it reads nothing here), and a
/// fresh read asked for. While there is none, whether one started is looked at every
/// 10 s (opening its snapshot: a few µs); one that stops answering for 5 s is let go.
pub fn readings() -> Option<Vec<Reading>> {
    let now = unsafe { GetTickCount64() };
    let mut c = CLIENT.lock().unwrap_or_else(|e| e.into_inner());
    if c.is_none() {
        let mut l = LOOKED.lock().unwrap_or_else(|e| e.into_inner());
        if *l != 0 && now < *l + 10_000 {
            return None;
        }
        *l = now;
        *c = Client::open();
    }
    let cl = c.as_mut()?;
    unsafe { SetEvent(cl.ask) };
    let Some((seq, st, v)) = read(cl.view) else {
        *c = None;
        return None;
    };
    if seq != cl.seq {
        (cl.seq, cl.stuck) = (seq, 0);
    } else if cl.stuck == 0 {
        cl.stuck = now;
    } else if now > cl.stuck + 5_000 {
        *c = None;
        return None;
    }
    (st == 0 && !v.is_empty()).then_some(v)
}

/// Is the service installed (and its state: running or not)?
pub fn installed() -> Option<bool> {
    unsafe {
        let m = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if m.is_null() {
            return None;
        }
        let s = OpenServiceW(m, wide(SERVICE).as_ptr(), SERVICE_QUERY_STATUS);
        let mut st: SERVICE_STATUS = std::mem::zeroed();
        let r = (!s.is_null()).then(|| QueryServiceStatus(s, &mut st) != 0 && st.dwCurrentState == SERVICE_RUNNING);
        if !s.is_null() {
            CloseServiceHandle(s);
        }
        CloseServiceHandle(m);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot() {
        let mut b = vec![0u64; SIZE / 8];
        let p = b.as_mut_ptr() as *mut u8;
        assert!(read(p).is_none());
        let rd = vec![
            Reading { label: "Pkg".into(), val: 47.0, minor: false },
            Reading { label: "Core 23".into(), val: 41.5, minor: true },
            Reading { label: "a label that is far too long to fit".into(), val: 1.0, minor: false },
        ];
        write(p, &rd, 0);
        let (s1, st, v) = read(p).unwrap();
        assert_eq!((st, v.len()), (0, 3));
        assert_eq!(v[..2], rd[..2]);
        assert_eq!(v[2].label, "a label that is far too l");
        write(p, &[], ST_NONE);
        let (s2, st, v) = read(p).unwrap();
        assert!(s2 > s1 && s2 % 2 == 0);
        assert_eq!((st, v.len()), (ST_NONE, 0));
        // (always being written: not read)
        unsafe { (*(p.add(8) as *const AtomicU32)).store(7, Ordering::Relaxed) };
        assert!(read(p).is_none());
    }

    /// The service's loop (with a reader that makes numbers up, in this session's
    /// namespace) and cpuview's side: asked, it reads and the client gets them.
    #[test]
    fn service_and_client() {
        std::env::set_var("CPUVIEW_SENSORS_NS", "Local");
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) } as usize;
        let t = std::thread::spawn(move || {
            let mut n = 0;
            serve_with(stop as HANDLE, &mut || {
                n += 1;
                Some(vec![Reading { label: "Pkg".into(), val: 40.0 + n as f32, minor: false }, Reading { label: "Core 0".into(), val: 39.5, minor: true }])
            })
        });
        // (the snapshot exists once the loop runs; the first look asks, a later one
        // has the answer)
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut got = None;
        for _ in 0..100 {
            if let Some(v) = readings() {
                got = Some(v);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let v = got.expect("the service's readings");
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].label.as_str(), v[0].minor), ("Pkg", false));
        assert!(v[0].val >= 41.0);
        assert_eq!((v[1].label.as_str(), v[1].val, v[1].minor), ("Core 0", 39.5, true));
        unsafe { SetEvent(stop as HANDLE) };
        assert_eq!(t.join().unwrap(), 0);
        *CLIENT.lock().unwrap() = None;
    }

    /// To see the service's readings in cpuview without the driver: made-up ones in
    /// this session for 2 minutes (cargo test --release fake_service -- --ignored),
    /// shown by a cpuview started with CPUVIEW_SENSORS_NS=Local.
    #[test]
    #[ignore]
    fn fake_service() {
        std::env::set_var("CPUVIEW_SENSORS_NS", "Local");
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) } as usize;
        let t0 = std::time::Instant::now();
        let t = std::thread::spawn(move || {
            serve_with(stop as HANDLE, &mut || {
                let w = (t0.elapsed().as_secs_f32() / 5.0).sin() * 8.0;
                let mut v = vec![Reading { label: "Pkg".into(), val: 52.0 + w, minor: false }];
                v.extend((0..24).map(|k| Reading { label: ["Core ", &k.to_string()].concat(), val: 45.0 + w + (k % 5) as f32, minor: true }));
                Some(v)
            })
        });
        std::thread::sleep(std::time::Duration::from_secs(120));
        unsafe { SetEvent(stop as HANDLE) };
        t.join().unwrap();
    }
}

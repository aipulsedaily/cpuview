//! Shared helpers for the Windows sources. Each of those is the Windows half of the
//! module it serves, declared there (`sys` → win/cpu.rs, `mem` → win/mem.rs …) so it
//! fills the same structures; `term` (the console) is loaded in place of term.rs.

pub mod board;
pub mod chips;
pub mod cputemp;
pub mod drive;
pub mod pawnio;
pub mod setup;
pub mod svc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{VirtualQuery, VirtualUnlock, MEMORY_BASIC_INFORMATION, MEM_IMAGE};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
use windows_sys::Win32::System::Threading::{WaitOnAddress, WakeByAddressSingle, INFINITE};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Mutex;

// (not in windows-sys: processor groups beyond the caller's)
windows_link::link!("ntdll.dll" "system" fn NtQuerySystemInformationEx(class: i32, input: *const core::ffi::c_void, inlen: u32, info: *mut core::ffi::c_void, len: u32, retlen: *mut u32) -> i32);
pub use windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation;

/// NUL-terminated UTF-16 for the W APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to the first NUL.
pub fn from_wide(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

/// A NUL-terminated UTF-16 string at `p`.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string.
pub unsafe fn from_pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// A registry string value (REG_SZ).
pub fn reg_str(root: HKEY, key: &str, val: &str) -> Option<String> {
    let mut b = [0u16; 256];
    let mut n = (b.len() * 2) as u32;
    let (k, v) = (wide(key), wide(val));
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), b.as_mut_ptr() as *mut _, &mut n)
    };
    (r == 0).then(|| from_wide(&b).trim().to_string())
}

/// A registry number (REG_DWORD).
pub fn reg_u32(root: HKEY, key: &str, val: &str) -> Option<u32> {
    let mut d = 0u32;
    let mut n = 4u32;
    let (k, v) = (wide(key), wide(val));
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut d as *mut u32 as *mut _, &mut n)
    };
    (r == 0).then_some(d)
}

/// A handle kept in an fd field (Disk::fd, Proc::fd …): 64-bit Windows keeps handle
/// values within 32 bits for interoperability, so they survive a round trip.
pub fn h2fd(h: HANDLE) -> i32 {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        -1
    } else {
        h as isize as i32
    }
}

pub fn fd2h(fd: i32) -> HANDLE {
    fd as isize as HANDLE
}

/// SYSTEM_PROCESS_INFORMATION (the full process list), up to the threads.
#[repr(C)]
pub struct Spi {
    pub next: u32,
    pub threads: u32,
    pub ws_private: i64,
    pub hard_faults: u32,
    pub threads_peak: u32,
    pub cycles: u64,
    pub create: i64,
    pub user: i64,
    pub kernel: i64,
    pub name_len: u16,
    pub name_max: u16,
    pub name: *const u16,
    pub prio: i32,
    pub pid: usize,
    pub ppid: usize,
    pub handles: u32,
    pub session: u32,
    pub key: usize,
    pub vm_peak: usize,
    pub vm: usize,
    pub faults: u32,
    pub ws_peak: usize,
    pub ws: usize,
    pub quota: [usize; 4],
    pub commit: usize,
    pub commit_peak: usize,
    pub private: usize,
    pub io: [i64; 6],
}

/// One process of a list: pid, parent, identity (sequence number or creation time:
/// pids are reused), name, and the full record when the list has one.
pub struct ProcEnt<'a> {
    pub pid: u32,
    pub ppid: u32,
    pub key: u64,
    pub name: &'a [u16],
    pub full: Option<&'a Spi>,
}

/// The system's process list. The basic list (pids, parents and names, Windows 11)
/// costs ~0.1–0.4 ms; the full one walks every thread of every process (16 ms with
/// 15 000 threads) and is read only when its numbers are needed.
pub struct ProcList {
    buf: Vec<u64>,
    len: usize,
    full: bool,
    /// the basic list isn't supported (Windows 10): every list is a full one
    pub no_basic: bool,
}

impl ProcList {
    pub fn new() -> ProcList {
        ProcList {
            buf: Vec::new(),
            len: 0,
            full: false,
            no_basic: false,
        }
    }

    /// Read the list (`full`: with sizes and times). False if it couldn't be read.
    pub fn read(&mut self, full: bool) -> bool {
        let full = full || self.no_basic;
        // SystemBasicProcessInformation (252), SystemProcessInformation (5)
        let class = if full { 5 } else { 252 };
        if self.buf.is_empty() {
            self.buf = vec![0u64; if full { 1 << 15 } else { 1 << 12 }];
        }
        loop {
            let mut got = 0u32;
            let st = unsafe {
                NtQuerySystemInformation(class, self.buf.as_mut_ptr() as *mut _, (self.buf.len() * 8) as u32, &mut got)
            };
            // STATUS_INFO_LENGTH_MISMATCH: grow (with room for new processes)
            if st == 0xc000_0004u32 as i32 && self.buf.len() < 1 << 22 {
                let want = (got as usize).div_ceil(8) + (got as usize).div_ceil(32) + 512;
                self.buf = vec![0u64; want.max(self.buf.len() * 2)];
                continue;
            }
            if st < 0 {
                if !full {
                    self.no_basic = true;
                    return self.read(true);
                }
                self.len = 0;
                return false;
            }
            self.len = got as usize;
            self.full = full;
            return true;
        }
    }

    /// Give the buffer back (the next read allocates it again).
    pub fn release(&mut self) {
        self.buf = Vec::new();
        self.len = 0;
    }

    pub fn each<'a>(&'a self, f: &mut dyn FnMut(ProcEnt<'a>)) {
        let p = self.buf.as_ptr() as *const u8;
        let mut o = 0usize;
        while self.len > 0 && o + 48 <= self.len {
            let e = unsafe { p.add(o) };
            let next = unsafe { *(e as *const u32) } as usize;
            let ent = if self.full {
                let s = unsafe { &*(e as *const Spi) };
                ProcEnt {
                    pid: s.pid as u32,
                    ppid: s.ppid as u32,
                    key: s.create as u64,
                    name: unsafe { wstr(s.name, s.name_len) },
                    full: Some(s),
                }
            } else {
                // SYSTEM_BASICPROCESS_INFORMATION: next, pid, parent, sequence number,
                // image name (UNICODE_STRING)
                let w = |k: usize| unsafe { *(e.add(8 * k) as *const usize) };
                ProcEnt {
                    pid: w(1) as u32,
                    ppid: w(2) as u32,
                    key: w(3) as u64,
                    name: unsafe { wstr(w(5) as *const u16, *(e.add(32) as *const u16)) },
                    full: None,
                }
            };
            f(ent);
            if next == 0 {
                break;
            }
            o += next;
        }
    }
}

/// A UNICODE_STRING's characters (`len` in bytes).
unsafe fn wstr<'a>(p: *const u16, len: u16) -> &'a [u16] {
    if p.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(p, len as usize / 2)
    }
}

/// The image name ends in ".exe" (the kernel's own processes have none).
pub fn is_exe(w: &[u16]) -> bool {
    let n = w.len();
    n > 4 && w[n - 4] == b'.' as u16 && w[n - 3..].iter().zip(b"exe").all(|(&c, &e)| c | 0x20 == e as u16)
}

/// A program's name for display: the image name without ".exe" (pid 0 is "Idle").
pub fn prog_name(w: &[u16], pid: u32) -> String {
    if w.is_empty() {
        return if pid == 0 { "Idle".into() } else { String::new() };
    }
    let s = String::from_utf16_lossy(w);
    let l = s.len();
    if l > 4 && s[l - 4..].eq_ignore_ascii_case(".exe") {
        s[..l - 4].to_string()
    } else {
        s
    }
}

pub fn close(h: HANDLE) {
    if !h.is_null() && h != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(h) };
    }
}

/// Handles of processes that may have ended, closed on a helper thread (started on
/// first use). Closing the last handle to an ended process frees what's left of it
/// (address space, handle table) in the closing thread: ~50–150 µs each, which a
/// scan after a minute would pay for every process that ended meanwhile. The
/// helper's time still counts (term::cpu_ns has every thread's), off the wakeups.
pub fn close_later(h: HANDLE) {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return;
    }
    CLOSING.lock().unwrap_or_else(|e| e.into_inner()).push(h as usize);
    CLOSE_ASK.fetch_add(1, Relaxed);
    if !CLOSER_UP.swap(true, Relaxed) {
        let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(closer);
    } else {
        unsafe { WakeByAddressSingle(CLOSE_ASK.as_ptr() as *const _) };
    }
}

static CLOSING: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static CLOSE_ASK: AtomicU32 = AtomicU32::new(0);
static CLOSER_UP: AtomicBool = AtomicBool::new(false);

fn closer() {
    loop {
        let a = CLOSE_ASK.load(Relaxed);
        let hs = std::mem::take(&mut *CLOSING.lock().unwrap_or_else(|e| e.into_inner()));
        for h in hs {
            close(h as HANDLE);
        }
        while CLOSE_ASK.load(Relaxed) == a {
            unsafe { WaitOnAddress(CLOSE_ASK.as_ptr() as *const _, &a as *const u32 as *const _, 4, INFINITE) };
        }
    }
}

/// Code pages out of the working set (see main.rs `trim_code`): every executable
/// image mapping (the program and its DLLs). VirtualUnlock on pages that aren't locked
/// removes them from the working set; they stay in memory and come back on use.
pub fn trim_code() {
    let mut a = 0usize;
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    while unsafe { VirtualQuery(a as *const _, &mut m, sz) } == sz {
        // PAGE_EXECUTE_READ (0x20) / PAGE_EXECUTE (0x10): code, not writable data
        if m.Type == MEM_IMAGE && matches!(m.Protect, 0x10 | 0x20) {
            unsafe { VirtualUnlock(m.BaseAddress, m.RegionSize) };
        }
        let next = (m.BaseAddress as usize).wrapping_add(m.RegionSize);
        if next <= a {
            break;
        }
        a = next;
    }
}

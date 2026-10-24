//! Windows console: VT output and input (SetConsoleMode), size, Ctrl-C / window close
//! and output; the same interface as the termios version (`term.rs`), so the renderer
//! and the input parser are shared. Input records are turned into the bytes a VT
//! terminal would send (keys, SGR mouse), resizes into WINCH.

use std::os::raw::c_int;
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, Ordering};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
use windows_sys::Win32::Foundation::{FILETIME, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::{GetSystemTimePreciseAsFileTime, GetTickCount64};
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};

pub static WINCH: AtomicBool = AtomicBool::new(false);
pub static QUIT: AtomicBool = AtomicBool::new(false);
pub static CONT: AtomicBool = AtomicBool::new(false);

/// Clock ids for `now_ns` (the values don't matter, only that they differ).
pub const CLOCK_REALTIME: c_int = 0;
pub const CLOCK_MONOTONIC: c_int = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: c_int = 2;
pub const CLOCK_THREAD_CPUTIME_ID: c_int = 3;
pub const CLOCK_BOOTTIME: c_int = 7;

/// Console modes and code page found at start (restored on exit), u32::MAX = unset.
static IN_MODE: AtomicU32 = AtomicU32::new(u32::MAX);
static OUT_MODE: AtomicU32 = AtomicU32::new(u32::MAX);
static OUT_CP: AtomicU32 = AtomicU32::new(0);
/// VT input was accepted (Windows 10 1607+): keys arrive as escape sequences
static VT_IN: AtomicBool = AtomicBool::new(false);
static QUIT_EV: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static TIMER: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// fd 0 / 1 / 2 → the standard handles
fn handle(fd: c_int) -> HANDLE {
    let id = match fd {
        0 => STD_INPUT_HANDLE,
        2 => STD_ERROR_HANDLE,
        _ => STD_OUTPUT_HANDLE,
    };
    unsafe { GetStdHandle(id) }
}

unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
    QUIT.store(true, Ordering::Relaxed);
    let ev = QUIT_EV.load(Ordering::Relaxed);
    if !ev.is_null() {
        SetEvent(ev);
    }
    if kind == CTRL_CLOSE_EVENT || kind == CTRL_LOGOFF_EVENT || kind == CTRL_SHUTDOWN_EVENT {
        // the process ends when this returns: give the main loop time to restore the
        // console and exit on its own
        Sleep(3000);
    }
    1
}

/// Ctrl-C arrives as a key (processed input is off); Ctrl-Break, closing the window,
/// log-off and shutdown end the main loop through QUIT and the quit event its wait
/// watches. Nothing is masked: the returned value only mirrors the termios version.
pub fn setup_signals() {
    unsafe {
        QUIT_EV.store(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()), Ordering::Relaxed);
        // a high-resolution timer: frame deadlines (16.7 ms at 60 fps) would otherwise
        // land on the 15.6 ms system tick
        let mut t = CreateWaitableTimerExW(
            std::ptr::null(),
            std::ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        );
        if t.is_null() {
            t = CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0, TIMER_ALL_ACCESS);
        }
        TIMER.store(t, Ordering::Relaxed);
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}

pub fn is_tty(fd: c_int) -> bool {
    let mut m = 0;
    unsafe { GetConsoleMode(handle(fd), &mut m) != 0 }
}

/// Window size in cells (the visible window, not the scroll-back buffer); pixel sizes
/// are unknown here (the probe asks the terminal). (The console's input handle has no
/// size: fd 0 asks the output.)
pub fn size(fd: c_int) -> (u16, u16, u16, u16) {
    unsafe {
        let mut i: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(handle(fd.max(1)), &mut i) != 0 {
            let w = i.srWindow.Right - i.srWindow.Left + 1;
            let h = i.srWindow.Bottom - i.srWindow.Top + 1;
            if w > 0 && h > 0 {
                return (w as u16, h as u16, 0, 0);
            }
        }
        (80, 24, 0, 0)
    }
}

/// Raw input with VT sequences (keys, mouse, resize records) on `fd`, VT output on
/// stdout, UTF-8 output (braille, blocks). Modes are restored by `restore_mode`.
pub fn raw_mode(fd: c_int) -> bool {
    unsafe {
        let (hin, hout) = (handle(fd), handle(1));
        let (mut im, mut om) = (0, 0);
        if GetConsoleMode(hin, &mut im) == 0 {
            return false;
        }
        if IN_MODE.load(Ordering::Relaxed) == u32::MAX {
            IN_MODE.store(im, Ordering::Relaxed);
            if GetConsoleMode(hout, &mut om) != 0 {
                OUT_MODE.store(om, Ordering::Relaxed);
            }
            OUT_CP.store(GetConsoleOutputCP(), Ordering::Relaxed);
        }
        // no line editing, echo or Ctrl-C processing; no quick-edit selection (it
        // would swallow clicks); window and mouse records
        let base = ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS;
        let vt = SetConsoleMode(hin, base | ENABLE_VIRTUAL_TERMINAL_INPUT) != 0;
        if !vt {
            SetConsoleMode(hin, base);
        }
        VT_IN.store(vt, Ordering::Relaxed);
        let om = OUT_MODE.load(Ordering::Relaxed);
        if om != u32::MAX {
            let want = (om | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING)
                & !ENABLE_WRAP_AT_EOL_OUTPUT;
            if SetConsoleMode(hout, want | DISABLE_NEWLINE_AUTO_RETURN) == 0 {
                SetConsoleMode(hout, want);
            }
        }
        SetConsoleOutputCP(65001);
        true
    }
}

pub fn restore_mode() {
    unsafe {
        let im = IN_MODE.load(Ordering::Relaxed);
        if im != u32::MAX {
            SetConsoleMode(handle(0), im);
        }
        let om = OUT_MODE.load(Ordering::Relaxed);
        if om != u32::MAX {
            SetConsoleMode(handle(1), om);
        }
        let cp = OUT_CP.load(Ordering::Relaxed);
        if cp != 0 {
            SetConsoleOutputCP(cp);
        }
    }
}

/// The console went away (its host ended: a closed tab, a crashed terminal): end
/// the main loop, as a read of 0 bytes does on Linux.
fn gone() {
    QUIT.store(true, Ordering::Relaxed);
}

pub fn write_all(fd: c_int, mut b: &[u8]) {
    let h = handle(fd);
    while !b.is_empty() {
        let mut n = 0u32;
        let len = b.len().min(1 << 20) as u32;
        if unsafe { WriteFile(h, b.as_ptr(), len, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
            gone();
            return;
        }
        b = &b[n as usize..];
    }
}

pub const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[?7l\x1b[2J";
pub const LEAVE: &str = "\x1b[0m\x1b[?1006l\x1b[?1000l\x1b[?7h\x1b[?25h\x1b[?1049l";

static QPC_FREQ: AtomicI64 = AtomicI64::new(0);

fn qpc_ns() -> i64 {
    let mut f = QPC_FREQ.load(Ordering::Relaxed);
    if f == 0 {
        unsafe { QueryPerformanceFrequency(&mut f) };
        f = f.max(1);
        QPC_FREQ.store(f, Ordering::Relaxed);
    }
    let mut c = 0i64;
    unsafe { QueryPerformanceCounter(&mut c) };
    (c / f) * 1_000_000_000 + (c % f) * 1_000_000_000 / f
}

/// Nanoseconds of the clock `clock` (QPC for the monotonic clock: no system call on
/// machines with an invariant TSC).
#[inline]
pub fn now_ns(clock: c_int) -> i64 {
    match clock {
        CLOCK_MONOTONIC => qpc_ns(),
        CLOCK_REALTIME => unsafe {
            let mut ft: FILETIME = std::mem::zeroed();
            GetSystemTimePreciseAsFileTime(&mut ft);
            let t = (ft.dwHighDateTime as i64) << 32 | ft.dwLowDateTime as i64;
            (t - 116_444_736_000_000_000) * 100
        },
        CLOCK_BOOTTIME => unsafe { GetTickCount64() as i64 * 1_000_000 },
        CLOCK_THREAD_CPUTIME_ID => thread_ns(),
        _ => process_ns(),
    }
}

#[cfg(target_arch = "x86_64")]
fn thread_ns() -> i64 {
    let mut c = 0u64;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    cycles_ns(c)
}

#[cfg(target_arch = "x86_64")]
fn process_ns() -> i64 {
    cycles_ns(process_cycles())
}

/// The main thread (a real handle: its time is read from wherever cpu_ns is called).
#[cfg(target_arch = "x86_64")]
static MAIN: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Cycles spent outside the main thread (helper threads, threads of system DLLs):
/// charged so far, found and not charged yet; the main thread's cycles at the last
/// reconciliation and in the minute before it; its time (0: none yet).
#[cfg(target_arch = "x86_64")]
static OTHER: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static SPREAD: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static M_REC: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static M_MIN: AtomicU64 = AtomicU64::new(1);
#[cfg(target_arch = "x86_64")]
static REC_NS: AtomicI64 = AtomicI64::new(0);

/// Process CPU cycles: the main thread's (0.1 µs to read) plus the rest of the
/// process as of the last QueryProcessCycleTime, which costs 50–150 µs and so runs
/// once a minute. What the other threads used is charged on the main thread's work
/// in the next minute, in proportion to it (at the last minute's rate; what's left
/// carries over): never lost, and neither in the one wakeup that finds it nor in
/// the one after a long sleep (the eco planner would take that for a sample's cost).
#[cfg(target_arch = "x86_64")]
fn process_cycles() -> u64 {
    unsafe {
        let mut h = MAIN.load(Ordering::Relaxed);
        if h.is_null() {
            let me = GetCurrentProcess();
            DuplicateHandle(me, GetCurrentThread(), me, &mut h, 0, 0, DUPLICATE_SAME_ACCESS);
            MAIN.store(h, Ordering::Relaxed);
        }
        let mut m = 0u64;
        QueryThreadCycleTime(h, &mut m);
        let spread = SPREAD.load(Ordering::Relaxed);
        let ran = m.saturating_sub(M_REC.load(Ordering::Relaxed));
        let mut part = (spread as u128 * ran as u128 / M_MIN.load(Ordering::Relaxed) as u128).min(spread as u128) as u64;
        let now = qpc_ns();
        let rec = REC_NS.load(Ordering::Relaxed);
        if rec == 0 || now - rec >= 60_000_000_000 {
            let done = OTHER.load(Ordering::Relaxed) + part;
            let mut p = 0u64;
            QueryProcessCycleTime(GetCurrentProcess(), &mut p);
            OTHER.store(done, Ordering::Relaxed);
            SPREAD.store(p.saturating_sub(m).saturating_sub(done), Ordering::Relaxed);
            M_MIN.store(ran.max(1), Ordering::Relaxed);
            M_REC.store(m, Ordering::Relaxed);
            REC_NS.store(now, Ordering::Relaxed);
            part = 0;
        }
        m + OTHER.load(Ordering::Relaxed) + part
    }
}

/// TSC → ns, measured against QPC from the first call on (fixed once a second has
/// passed; 3 GHz for the first 20 ms). Process and thread times count TSC cycles
/// exactly (GetProcessTimes only moves on the 15.6 ms clock tick, too coarse for
/// wakeups of ~50 µs).
#[cfg(target_arch = "x86_64")]
static TSC0: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static NS0: AtomicI64 = AtomicI64::new(0);
/// ns per 2^32 cycles, once measured
#[cfg(target_arch = "x86_64")]
static RATIO: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
fn cycles_ns(c: u64) -> i64 {
    let r = RATIO.load(Ordering::Relaxed);
    let r = if r != 0 {
        r
    } else {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        let ns = qpc_ns();
        if TSC0.load(Ordering::Relaxed) == 0 {
            TSC0.store(tsc, Ordering::Relaxed);
            NS0.store(ns, Ordering::Relaxed);
        }
        let (dt, dn) = (tsc - TSC0.load(Ordering::Relaxed), ns - NS0.load(Ordering::Relaxed));
        if dn >= 1_000_000_000 {
            let r = ((dn as u128) << 32) / dt.max(1) as u128;
            RATIO.store(r as u64, Ordering::Relaxed);
            r as u64
        } else if dn >= 20_000_000 {
            (((dn as u128) << 32) / dt.max(1) as u128) as u64
        } else {
            // the first 20 ms: 3 GHz (only start-up is measured with it)
            (1u64 << 32) / 3
        }
    };
    ((c as u128 * r as u128) >> 32) as i64
}

/// Other CPUs (ARM64): the cycle counts' rate isn't known here, and GetProcessTimes
/// moves in 15.6 ms ticks. The main thread's time awake (from a wakeup to the next
/// wait: a short wakeup's CPU time) stands in for the process's CPU time, wall time
/// for a thread's.
#[cfg(not(target_arch = "x86_64"))]
static AWAKE: AtomicI64 = AtomicI64::new(0);
#[cfg(not(target_arch = "x86_64"))]
static WOKE: AtomicI64 = AtomicI64::new(0);

#[cfg(not(target_arch = "x86_64"))]
fn thread_ns() -> i64 {
    qpc_ns()
}

#[cfg(not(target_arch = "x86_64"))]
fn process_ns() -> i64 {
    let now = qpc_ns();
    if WOKE.load(Ordering::Relaxed) == 0 {
        WOKE.store(now, Ordering::Relaxed);
    }
    AWAKE.load(Ordering::Relaxed) + now - WOKE.load(Ordering::Relaxed)
}

/// Going to sleep in `wait` (`true`) or waking up: the time awake, see process_ns.
fn sleeping(on: bool) {
    #[cfg(not(target_arch = "x86_64"))]
    {
        let now = qpc_ns();
        if on {
            let w = WOKE.load(Ordering::Relaxed);
            if w != 0 {
                AWAKE.fetch_add(now - w, Ordering::Relaxed);
            }
        }
        WOKE.store(now, Ordering::Relaxed);
    }
    let _ = on;
}

pub fn mono() -> i64 {
    qpc_ns()
}

/// Process CPU time (all threads), ns.
pub fn cpu_ns() -> i64 {
    now_ns(CLOCK_PROCESS_CPUTIME_ID)
}

/// Input turned into VT bytes, waiting for `read`, and a high surrogate waiting for
/// its pair.
struct Pend {
    b: [u8; 4096],
    n: usize,
    hi: u16,
}

static PEND: std::sync::Mutex<Pend> = std::sync::Mutex::new(Pend { b: [0; 4096], n: 0, hi: 0 });

fn pend() -> std::sync::MutexGuard<'static, Pend> {
    PEND.lock().unwrap_or_else(|e| e.into_inner())
}

fn push(b: &[u8]) {
    let mut p = pend();
    let at = p.n;
    let n = b.len().min(p.b.len() - at);
    p.b[at..at + n].copy_from_slice(&b[..n]);
    p.n += n;
}

fn push_char(u: u16) {
    let c = if (0xd800..0xdc00).contains(&u) {
        pend().hi = u;
        return;
    } else if (0xdc00..0xe000).contains(&u) {
        let hi = std::mem::take(&mut pend().hi);
        char::from_u32(0x10000 + (((hi as u32) & 0x3ff) << 10) + (u as u32 & 0x3ff))
    } else {
        char::from_u32(u as u32)
    };
    if let Some(c) = c {
        let mut b = [0u8; 4];
        push(c.encode_utf8(&mut b).as_bytes());
    }
}

/// Keys without a character when VT input isn't available (old consoles).
fn vk_seq(vk: u16, shift: bool) -> Option<&'static [u8]> {
    Some(match vk {
        0x26 => b"\x1b[A",
        0x28 => b"\x1b[B",
        0x27 => b"\x1b[C",
        0x25 => b"\x1b[D",
        0x24 => b"\x1b[H",
        0x23 => b"\x1b[F",
        0x21 => b"\x1b[5~",
        0x22 => b"\x1b[6~",
        0x2e => b"\x1b[3~",
        0x78 => b"\x1b[20~",
        0x09 if shift => b"\x1b[Z",
        _ => return None,
    })
}

/// Mouse records (consoles that don't send VT mouse input) as SGR reports: presses
/// and the wheel, as the parser takes them.
fn mouse(m: &MOUSE_EVENT_RECORD) {
    let btn = if m.dwEventFlags & MOUSE_WHEELED != 0 {
        if (m.dwButtonState as i32) > 0 {
            64
        } else {
            65
        }
    } else if m.dwEventFlags == 0 && m.dwButtonState & FROM_LEFT_1ST_BUTTON_PRESSED != 0 {
        0
    } else if m.dwEventFlags == 0 && m.dwButtonState & RIGHTMOST_BUTTON_PRESSED != 0 {
        2
    } else {
        return;
    };
    let mut f = crate::ui::F::new();
    f.str("\x1b[<");
    f.int(btn, 0);
    f.str(";");
    f.int(m.dwMousePosition.X.max(0) as u32 + 1, 0);
    f.str(";");
    f.int(m.dwMousePosition.Y.max(0) as u32 + 1, 0);
    f.str("M");
    push(f.s().as_bytes());
}

/// Read every waiting input record into PEND (keys, mouse) and WINCH (resizes).
fn drain(h: HANDLE) {
    let mut recs: [INPUT_RECORD; 64] = unsafe { std::mem::zeroed() };
    loop {
        let mut n = 0u32;
        if unsafe { GetNumberOfConsoleInputEvents(h, &mut n) } == 0 {
            gone();
            return;
        }
        if n == 0 {
            return;
        }
        let mut got = 0u32;
        if unsafe { ReadConsoleInputW(h, recs.as_mut_ptr(), recs.len() as u32, &mut got) } == 0 {
            gone();
            return;
        }
        for r in &recs[..got as usize] {
            match r.EventType as u32 {
                KEY_EVENT => {
                    let k = unsafe { r.Event.KeyEvent };
                    if k.bKeyDown == 0 {
                        continue;
                    }
                    let u = unsafe { k.uChar.UnicodeChar };
                    for _ in 0..k.wRepeatCount.max(1) {
                        if u != 0 {
                            push_char(u);
                        } else if !VT_IN.load(Ordering::Relaxed) {
                            let shift = k.dwControlKeyState & SHIFT_PRESSED != 0;
                            if let Some(s) = vk_seq(k.wVirtualKeyCode, shift) {
                                push(s);
                            }
                        }
                    }
                }
                MOUSE_EVENT => mouse(unsafe { &r.Event.MouseEvent }),
                WINDOW_BUFFER_SIZE_EVENT => WINCH.store(true, Ordering::Relaxed),
                _ => {}
            }
        }
    }
}

fn pending() -> bool {
    pend().n > 0
}

/// Wait for input on `fd` until `timeout_ns` (or a resize / quit). Returns true if
/// there are bytes to `read`.
pub fn wait(fd: c_int, timeout_ns: i64, _mask: &()) -> bool {
    sleeping(true);
    let r = wait_for(fd, timeout_ns);
    sleeping(false);
    r
}

fn wait_for(fd: c_int, timeout_ns: i64) -> bool {
    let h = handle(fd);
    let end = mono() + timeout_ns.max(0);
    let (timer, quit) = (TIMER.load(Ordering::Relaxed), QUIT_EV.load(Ordering::Relaxed));
    loop {
        if pending() {
            return true;
        }
        if WINCH.load(Ordering::Relaxed) || QUIT.load(Ordering::Relaxed) {
            return false;
        }
        let left = end - mono();
        let mut hs = [h, quit, timer];
        let mut n = if quit.is_null() { 1 } else { 2 };
        let ms = if left <= 0 {
            0
        } else if !timer.is_null() {
            // relative due time in 100 ns units
            let due = -(left / 100).max(1);
            unsafe { SetWaitableTimer(timer, &due, 0, None, std::ptr::null(), 0) };
            if quit.is_null() {
                hs[1] = timer;
            }
            n += 1;
            INFINITE
        } else {
            ((left + 999_999) / 1_000_000) as u32
        };
        let r = unsafe { WaitForMultipleObjects(n, hs.as_ptr(), 0, ms) };
        if r == WAIT_FAILED {
            gone();
            return false;
        }
        if r == WAIT_OBJECT_0 {
            drain(h);
        } else if r == WAIT_OBJECT_0 + 1 && !quit.is_null() {
            return false;
        } else {
            return pending();
        }
        if left <= 0 {
            return pending();
        }
    }
}

pub fn read(_fd: c_int, buf: &mut [u8]) -> isize {
    let mut p = pend();
    let (have, n) = (p.n, p.n.min(buf.len()));
    buf[..n].copy_from_slice(&p.b[..n]);
    p.b.copy_within(n..have, 0);
    p.n -= n;
    n as isize
}

/// Throw away waiting input (the probe's late replies, for --doctor).
pub fn flush_input(fd: c_int) {
    unsafe { FlushConsoleInputBuffer(handle(fd)) };
    pend().n = 0;
}

/// Ctrl-Z: Windows consoles have no job control; nothing to do.
pub fn suspend(_fd: c_int, _out: c_int) {}

/// Local time of day (h, m, s) from a realtime timestamp.
pub fn local_hms(secs: i64) -> (i32, i32, i32) {
    unsafe {
        let t = (secs * 10_000_000 + 116_444_736_000_000_000) as u64;
        let ft = FILETIME {
            dwLowDateTime: t as u32,
            dwHighDateTime: (t >> 32) as u32,
        };
        let (mut utc, mut loc) = (std::mem::zeroed(), std::mem::zeroed());
        if FileTimeToSystemTime(&ft, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0
        {
            return (0, 0, 0);
        }
        (loc.wHour as i32, loc.wMinute as i32, loc.wSecond as i32)
    }
}

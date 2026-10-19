//! Raw terminal mode (termios), size, signals and output.

use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

pub static WINCH: AtomicBool = AtomicBool::new(false);
pub static QUIT: AtomicBool = AtomicBool::new(false);
pub static CONT: AtomicBool = AtomicBool::new(false);
static TTY_FD: AtomicI32 = AtomicI32::new(-1);
static mut ORIG: Option<libc::termios> = None;

extern "C" fn on_signal(sig: c_int) {
    match sig {
        libc::SIGWINCH => WINCH.store(true, Ordering::Relaxed),
        libc::SIGCONT => CONT.store(true, Ordering::Relaxed),
        _ => QUIT.store(true, Ordering::Relaxed),
    }
}

pub const SIGS: [c_int; 6] = [
    libc::SIGWINCH,
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGCONT,
];

/// Install handlers and block the signals; they are delivered only inside ppoll()
/// (whose mask unblocks them), so the main loop never races with a handler.
pub fn setup_signals() -> libc::sigset_t {
    unsafe {
        let mut block: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut block);
        for s in SIGS {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(s, &sa, std::ptr::null_mut());
            libc::sigaddset(&mut block, s);
        }
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, &block, &mut old);
        for s in SIGS {
            libc::sigdelset(&mut old, s);
        }
        old
    }
}

pub fn is_tty(fd: c_int) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn size(fd: c_int) -> (u16, u16, u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            (ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel)
        } else {
            (80, 24, 0, 0)
        }
    }
}

pub fn raw_mode(fd: c_int) -> bool {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return false;
        }
        #[allow(static_mut_refs)]
        if ORIG.is_none() {
            ORIG = Some(t);
        }
        TTY_FD.store(fd, Ordering::Relaxed);
        t.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON);
        t.c_oflag &= !libc::OPOST;
        t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
        t.c_cflag &= !(libc::CSIZE | libc::PARENB);
        t.c_cflag |= libc::CS8;
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(fd, libc::TCSANOW, &t) == 0
    }
}

pub fn restore_mode() {
    let fd = TTY_FD.load(Ordering::Relaxed);
    #[allow(static_mut_refs)]
    unsafe {
        if let (true, Some(t)) = (fd >= 0, ORIG.as_ref()) {
            libc::tcsetattr(fd, libc::TCSANOW, t);
        }
    }
}

pub fn write_all(fd: c_int, mut b: &[u8]) {
    while !b.is_empty() {
        let n = unsafe { libc::write(fd, b.as_ptr() as *const _, b.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error().raw_os_error();
            if e == Some(libc::EINTR) || e == Some(libc::EAGAIN) {
                if e == Some(libc::EAGAIN) {
                    let mut p = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut p, 1, 100) };
                }
                continue;
            }
            return;
        }
        b = &b[n as usize..];
    }
}

pub const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[?7l\x1b[2J";
pub const LEAVE: &str = "\x1b[0m\x1b[?1006l\x1b[?1000l\x1b[?7h\x1b[?25h\x1b[?1049l";

/// Monotonic time in nanoseconds (vDSO, no syscall).
#[inline]
pub fn now_ns(clock: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

pub fn mono() -> i64 {
    now_ns(libc::CLOCK_MONOTONIC)
}

/// Process CPU time (all threads), ns.
pub fn cpu_ns() -> i64 {
    now_ns(libc::CLOCK_PROCESS_CPUTIME_ID)
}

/// Wait for input on `fd` until `timeout_ns` (or a signal). Returns true if readable.
pub fn wait(fd: c_int, timeout_ns: i64, mask: &libc::sigset_t) -> bool {
    let t = timeout_ns.max(0);
    let ts = libc::timespec {
        tv_sec: (t / 1_000_000_000) as _,
        tv_nsec: (t % 1_000_000_000) as _,
    };
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::ppoll(&mut p, 1, &ts, mask) };
    r > 0 && p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

pub fn read(fd: c_int, buf: &mut [u8]) -> isize {
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) }
}

/// Suspend (Ctrl-Z): restore the terminal, stop, and come back in raw mode.
pub fn suspend(fd: c_int, out: c_int) {
    write_all(out, LEAVE.as_bytes());
    restore_mode();
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTSTP);
        libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        libc::kill(0, libc::SIGTSTP);
    }
    raw_mode(fd);
    write_all(out, ENTER.as_bytes());
}

/// Local time of day (h, m, s) from a realtime timestamp.
pub fn local_hms(secs: i64) -> (i32, i32, i32) {
    unsafe {
        let t = secs as _;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

//! Processes on Windows (the Windows half of procs.rs). First step: nothing listed
//! yet, no actions.

use super::*;

pub struct State;

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
            users: Vec::new(),
            gen: 0,
            scans: 0,
            hz: 1e7,
            page: 4096,
            show_k: false,
            io_all: false,
            nk: 0,
            denied: 0,
            hide: 0,
            me: 0,
            load: [0.0; 3],
            scan_t: 0,
            cost: 0,
            last_track: i64::MIN / 2,
            ok: false,
            statm: true,
            w: State,
        }
    }

    pub fn scan(&mut self, _now: i64, _keep_ms: u32) {}

    pub fn read_load(&mut self) {}

    pub fn track(&mut self, _want: &[(u32, u64)]) {}

    pub fn read_tracked(&mut self, _now: i64, _io: bool) {}

    pub fn start_of(&mut self, _pid: u32) -> Option<u64> {
        None
    }

    pub fn detail(&mut self, pid: u32, start: u64, now: i64) -> Detail {
        Detail {
            pid,
            start,
            t: now,
            ..Default::default()
        }
    }

    pub fn release(&mut self) {}

    pub fn signal(&mut self, _pid: u32, _start: u64, _sig: c_int) -> Result<(), c_int> {
        Err(EPERM)
    }

    pub fn renice(&mut self, _pid: u32, _start: u64, _nice: i32) -> Result<(), c_int> {
        Err(EPERM)
    }
}

pub const NSIG: usize = 1;
pub fn sig(_i: usize) -> (&'static str, c_int, &'static str) {
    ("END", 0, "ends it at once (no clean-up)")
}

pub fn why(_e: c_int) -> &'static str {
    "not supported yet"
}

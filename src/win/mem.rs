//! Memory & storage on Windows (the Windows half of mem.rs). First step: nothing read
//! yet.

use super::*;

pub struct State;

impl State {
    pub fn new() -> State {
        State
    }
}

pub(super) fn start_slow() {}

pub(super) fn wake_slow() {}

impl Fs {
    fn stat(&mut self) {}
}

impl Mem {
    pub fn new() -> Mem {
        Mem::empty()
    }

    pub fn refresh_fs(&mut self) {
        for f in &mut self.fs {
            f.stat();
        }
    }

    pub(super) fn read_now(&mut self, _now: i64) {}

    pub fn shrink(&mut self) {}

    pub fn scan_procs(&mut self) {}

    pub fn need_scan(&mut self) -> bool {
        false
    }

    pub fn refresh_procs(&mut self) {}
}

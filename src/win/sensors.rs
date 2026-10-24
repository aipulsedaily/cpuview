//! Sensors on Windows (the Windows half of sensors.rs). First step: nothing found yet.

use super::*;

pub struct State;

impl State {
    pub fn new() -> State {
        State
    }
}

impl Sens {
    pub fn detect(&mut self, _now: i64) {
        self.detected = true;
    }

    pub(super) fn read_bats(&mut self, _now: i64) {}

    pub(super) fn read_win(&mut self, _i: usize) {}

    pub(super) fn read_bmc(&mut self, _i: usize, _wait_ms: i32) {}
}

/// `cpuview --setup-sensors` on Windows: nothing to set up.
pub fn setup(_undo: bool, _per_core: bool, _bmc: bool, _dry: bool) -> i32 {
    println!("cpuview: --setup-sensors has nothing to set up on Windows");
    0
}

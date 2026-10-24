//! Network on Windows (the Windows half of net.rs). First step: nothing read yet.

use super::*;

impl Net {
    pub fn new() -> Net {
        Net::empty()
    }

    pub(super) fn read_dev(&mut self, now: i64) {
        self.last_dev = now;
        let push = std::mem::take(&mut self.held);
        if push > 0 {
            self.hist_n += 1;
            self.pts += push as u64;
        }
    }

    pub(super) fn links(&mut self) {}

    pub(super) fn addrs(&mut self) {}

    pub(super) fn read_counts(&mut self) {}

    pub(super) fn read_listen(&mut self) {}

    pub(super) fn read_peers(&mut self) {}

    pub(super) fn scan_step(&mut self, _n: usize) {}
}

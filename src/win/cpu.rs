//! CPU on Windows (the Windows half of sys.rs). First step: the topology's CPU count
//! only, nothing sampled yet.

use crate::sys::Topo;

pub struct Cpu;

impl Cpu {
    /// Reads nothing (tests).
    #[cfg(test)]
    pub fn none() -> Cpu {
        Cpu
    }

    pub fn new(_t: &Topo) -> Cpu {
        Cpu
    }

    pub fn times(&mut self, _cur: &mut [(u64, u64)]) {}

    pub fn clocks(&mut self, _ur: &[f32], _fr: &mut [f32], _t: &mut Topo) {}

    pub fn loadavg(&mut self, _busy: f32, _now: i64, _la: &mut [f32; 3]) {}

    pub fn policy(&mut self, _gov: &mut String) {}
}

pub fn topo() -> Topo {
    let n = std::thread::available_parallelism().map_or(1, |n| n.get());
    let cpus: Vec<usize> = (0..n).collect();
    Topo {
        idx: cpus.clone(),
        groups: (0..n).map(|c| vec![c]).collect(),
        ccd: vec![0; n],
        ndom: 1,
        rank: vec![-1; n],
        maxf: 1.0,
        minf: 0.0,
        boost: 0.9,
        model: "CPU".into(),
        short_model: "CPU".into(),
        driver: String::new(),
        has_freq: false,
        cpus,
    }
}

pub fn auto_pin(_t: &Topo) {}

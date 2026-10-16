//! Opt-in phase profiler (build with `--features prof`, run with CPUVIEW_PROFILE=path):
//! thread CPU ns per phase, written at exit. Compiled out by default.

use crate::term;

const N: usize = 17;
static mut PROF: [i64; N] = [0; N];
static mut ON: bool = false;
static mut CPU0: i64 = 0;
const NAMES: [&str; N] = [
    "sample",
    "draw",
    "flush",
    "write",
    "temps",
    "slow",
    "ticks",
    "frames",
    "d.title",
    "d.graphs",
    "d.sensors",
    "d.cores",
    "d.detail",
    "d.sort",
    "anim",
    "d.mem",
    "mem",
];

pub fn init() {
    unsafe { ON = cfg!(feature = "prof") && std::env::var_os("CPUVIEW_PROFILE").is_some() };
}

#[inline]
pub fn prof<T>(k: usize, f: impl FnOnce() -> T) -> T {
    if !cfg!(feature = "prof") || unsafe { !ON } {
        return f();
    }
    let t0 = term::now_ns(libc::CLOCK_THREAD_CPUTIME_ID);
    let r = f();
    unsafe { PROF[k] += term::now_ns(libc::CLOCK_THREAD_CPUTIME_ID) - t0 };
    r
}

pub fn prof_count(k: usize) {
    unsafe {
        if !cfg!(feature = "prof") || !ON {
            return;
        }
        PROF[k] += 1;
        // steady state only: forget the first 5 frames (start-up, first full paint)
        if k == 7 && PROF[7] == 5 && CPU0 == 0 {
            PROF = [0; N];
            CPU0 = term::cpu_ns();
        }
    }
}

pub fn dump(rate: usize) {
    if !cfg!(feature = "prof") {
        return;
    }
    if let Some(p) = std::env::var_os("CPUVIEW_PROFILE") {
        let mut r = String::new();
        #[allow(static_mut_refs)]
        let v = unsafe { PROF };
        for (k, v) in v.iter().enumerate() {
            if NAMES[k] != "-" {
                r += &format!("{} {}\n", NAMES[k], v);
            }
        }
        r += &format!(
            "steady_cpu_ns {}\nrate_index {}\n",
            term::cpu_ns() - unsafe { CPU0 },
            rate
        );
        let _ = std::fs::write(p, r);
    }
}

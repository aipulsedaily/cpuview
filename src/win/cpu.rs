//! CPU on Windows: busy and idle time per logical CPU (NtQuerySystemInformation, one
//! call per processor group), effective clocks (PDH "% Processor Performance": the
//! clock each CPU really ran at over the interval, as a share of its nominal clock,
//! from the APERF / MPERF counters the kernel reads), topology
//! (GetLogicalProcessorInformationEx), the power plan, and a load average (Windows has
//! none: estimated from the busy CPUs).

use crate::win::{from_wide, wide, NtQuerySystemInformationEx};
use crate::sensors::cpu_temp_label;
use crate::sys::{isort, Ring, Sensor, Topo};
use crate::win::{lhm, svc};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Power::{CallNtPowerInformation, PowerGetActiveScheme, PowerReadFriendlyName, ProcessorInformation};
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::{GetActiveProcessorGroupCount, GetCurrentProcess, GetProcessAffinityMask, SetProcessAffinityMask};

/// SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION (100 ns units; kernel time includes idle)
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sppi {
    idle: i64,
    kernel: i64,
    user: i64,
    dpc: i64,
    interrupt: i64,
    count: u32,
}

const PDH_FMT_NOCAP100: u32 = 0x8000;
const PERF: &str = "\\Processor Information(*)\\% Processor Performance";

pub struct Cpu {
    /// per processor group: (group number, its first CPU index, its CPU count)
    groups: Vec<(u16, usize, usize)>,
    buf: Vec<Sppi>,
    /// effective clocks: query, counter, array buffer, item → CPU index
    q: PDH_HQUERY,
    ctr: PDH_HCOUNTER,
    arr: Vec<u64>,
    map: Vec<usize>,
    /// nominal clock (MHz): "% Processor Performance" is relative to it
    nominal: f32,
    /// highest clock believed (MHz): a fifth above the rated maximum where the CPU
    /// states one (not under Windows' hypervisor, which hides it)
    ceil: f32,
    /// CPUs whose clock was read this time
    read: Vec<bool>,
    /// load average: the time of its last update
    la_t: i64,
    /// LibreHardwareMonitor's CPU temperatures: when a fresh snapshot was last asked for
    lhm_t: i64,
    /// the temperatures came from cpuview's sensor service (win/svc.rs)
    from_svc: bool,
    /// active power plan
    plan: windows_sys::core::GUID,
}

/// Logical processor records of one kind (the whole buffer, walked by the caller).
fn glpi(rel: LOGICAL_PROCESSOR_RELATIONSHIP) -> Vec<u64> {
    let mut len = 0u32;
    unsafe { GetLogicalProcessorInformationEx(rel, std::ptr::null_mut(), &mut len) };
    let mut b = vec![0u64; (len as usize).div_ceil(8)];
    if unsafe { GetLogicalProcessorInformationEx(rel, b.as_mut_ptr() as *mut _, &mut len) } == 0 {
        b.clear();
    }
    b.truncate((len as usize).div_ceil(8));
    b
}

/// Call `f` for every record in a `glpi` buffer.
fn each(b: &[u64], f: &mut dyn FnMut(&SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)) {
    let (p, len) = (b.as_ptr() as *const u8, b.len() * 8);
    let mut o = 0usize;
    while o + 8 <= len {
        let r = unsafe { &*(p.add(o) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
        if r.Size == 0 {
            break;
        }
        f(r);
        o += r.Size as usize;
    }
}

/// CPU indices (in `first`'s numbering) of the processors in group masks.
fn cpus_of(masks: *const GROUP_AFFINITY, n: usize, first: &[(u16, usize)], out: &mut Vec<usize>) {
    for k in 0..n {
        let m = unsafe { *masks.add(k) };
        let Some(&(_, base)) = first.iter().find(|g| g.0 == m.Group) else {
            continue;
        };
        for bit in 0..usize::BITS as usize {
            if m.Mask >> bit & 1 != 0 {
                out.push(base + bit);
            }
        }
    }
}

/// (group, its first CPU index) for every active group, and the CPU count; CPUs are
/// numbered group by group, as Task Manager does.
fn group_bases() -> (Vec<(u16, usize)>, usize) {
    let (mut v, mut n) = (Vec::new(), 0usize);
    each(&glpi(RelationGroup), &mut |r| {
        let g = unsafe { &r.Anonymous.Group };
        let info = g.GroupInfo.as_ptr();
        for k in 0..g.ActiveGroupCount as usize {
            let gi = unsafe { *info.add(k) };
            v.push((k as u16, n));
            n += gi.ActiveProcessorCount as usize;
        }
    });
    if v.is_empty() {
        v.push((0, 0));
        n = std::thread::available_parallelism().map_or(1, |n| n.get());
    }
    (v, n)
}

/// Processor group and number in it of each CPU (cpuview's CPU index: the groups'
/// CPUs one after the other).
pub fn group_numbers() -> Vec<(u16, u8)> {
    let (bases, n) = group_bases();
    (0..n)
        .map(|c| {
            let k = bases.iter().rposition(|b| b.1 <= c).unwrap_or(0);
            (bases[k].0, (c - bases[k].1) as u8)
        })
        .collect()
}

/// Nominal clock (MHz) of the first processor.
fn nominal_mhz(n: usize) -> f32 {
    // PROCESSOR_POWER_INFORMATION: Number, MaxMhz, CurrentMhz, MhzLimit, MaxIdleState,
    // CurrentIdleState (u32 each)
    let mut b = vec![[0u32; 6]; n.max(1)];
    let len = (b.len() * 24) as u32;
    let r = unsafe { CallNtPowerInformation(ProcessorInformation, std::ptr::null(), 0, b.as_mut_ptr() as *mut _, len) };
    if r == 0 && b[0][1] > 0 {
        return b[0][1] as f32;
    }
    crate::win::reg_u32(HKEY_LOCAL_MACHINE, "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0", "~MHz").unwrap_or(0) as f32
}

/// Highest rated clock (MHz) where the CPU states it (CPUID leaf 0x16, Intel).
fn rated_max() -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::__cpuid;
        if __cpuid(0).eax >= 0x16 {
            return (__cpuid(0x16).ebx & 0xffff) as f32;
        }
    }
    0.0
}

/// The CPU's temperatures where LibreHardwareMonitor's web server runs (Windows has
/// none of its own): the package (Intel), Tctl and the CCDs' (AMD).
pub fn find_sensors() -> Vec<Sensor> {
    if !lhm::listening() {
        return Vec::new();
    }
    let Some(v) = lhm::fetch() else {
        return Vec::new();
    };
    let out = cpu_sensors(&v);
    lhm::set(v);
    out
}

/// The CPU's temperatures among LibreHardwareMonitor's readings.
fn cpu_sensors(v: &[lhm::Reading]) -> Vec<Sensor> {
    let mut out = Vec::new();
    for x in v {
        if let (true, Some(l)) = (x.typ == "Temperature" && x.hwid.contains("cpu"), cpu_temp_label(&x.text)) {
            if !out.iter().any(|s: &Sensor| s.label == l) {
                out.push(Sensor {
                    label: l,
                    fd: -1,
                    val: if x.val.is_finite() { x.val } else { 0.0 },
                    hist: Ring::new(),
                    max: 0.0,
                });
            }
        }
    }
    out
}

pub fn topo() -> Topo {
    let (bases, n) = group_bases();
    let cpus: Vec<usize> = (0..n).collect();
    let idx = cpus.clone();
    // physical cores (their threads) and efficiency classes
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut eff: Vec<u8> = Vec::new();
    each(&glpi(RelationProcessorCore), &mut |r| {
        let p = unsafe { &r.Anonymous.Processor };
        let mut g = Vec::new();
        cpus_of(p.GroupMask.as_ptr(), p.GroupCount as usize, &bases, &mut g);
        g.retain(|&c| c < n);
        if !g.is_empty() {
            groups.push(g);
            eff.push(p.EfficiencyClass);
        }
    });
    // every CPU in exactly one core (else one core per CPU)
    let mut seen = vec![false; n];
    for g in &groups {
        for &c in g {
            seen[c] = true;
        }
    }
    if groups.is_empty() || seen.iter().any(|s| !s) {
        groups = (0..n).map(|c| vec![c]).collect();
        eff = vec![0; n];
    }
    let mut order: Vec<usize> = (0..groups.len()).collect();
    isort(&mut order, &|a, b| groups[*a][0] < groups[*b][0]);
    let groups: Vec<Vec<usize>> = order.iter().map(|&i| groups[i].clone()).collect();
    let eff: Vec<u8> = order.iter().map(|&i| eff[i]).collect();
    // L3 domains: the CPUs each last-level cache serves
    let mut l3: Vec<Vec<usize>> = Vec::new();
    each(&glpi(RelationCache), &mut |r| {
        let c = unsafe { &r.Anonymous.Cache };
        if c.Level == 3 {
            let mut v = Vec::new();
            let masks = unsafe { c.Anonymous.GroupMasks.as_ptr() };
            cpus_of(masks, (c.GroupCount as usize).max(1), &bases, &mut v);
            l3.push(v);
        }
    });
    let mut ccd = Vec::with_capacity(groups.len());
    let mut doms: Vec<usize> = Vec::new();
    for g in &groups {
        let d = l3.iter().position(|v| v.contains(&g[0])).unwrap_or(usize::MAX);
        let k = doms.iter().position(|&x| x == d).unwrap_or_else(|| {
            doms.push(d);
            doms.len() - 1
        });
        ccd.push(k);
    }
    // the scheduler's preference: efficiency class (hybrid CPUs: performance cores
    // first), then the CPU set's scheduling class (preferred cores); -1 where all equal
    let sched = sched_classes(n, &bases);
    let mut rank: Vec<i64> = groups
        .iter()
        .zip(&eff)
        .map(|(g, &e)| (e as i64) << 8 | g.iter().map(|&c| sched[c] as i64).max().unwrap_or(0))
        .collect();
    if rank.iter().all(|&r| r == rank[0]) {
        rank.iter_mut().for_each(|r| *r = -1);
    }
    let nominal = nominal_mhz(n);
    let has_freq = nominal > 0.0;
    let maxf = if has_freq { nominal.max(rated_max()) } else { 1.0 };
    let model = crate::win::reg_str(HKEY_LOCAL_MACHINE, "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0", "ProcessorNameString")
        .unwrap_or_else(|| "CPU".into());
    let short_model = model
        .replace("(R)", "")
        .replace("(TM)", "")
        .split_whitespace()
        .filter(|w| *w != "Processor" && *w != "CPU" && !w.ends_with("-Core"))
        .collect::<Vec<_>>()
        .join(" ");
    Topo {
        cpus,
        idx,
        groups,
        ccd,
        ndom: doms.len().max(1),
        rank,
        maxf,
        // (raised to the slowest clock seen: idle CPUs show it, as on Linux)
        minf: 0.0,
        boost: 0.9 * maxf,
        model,
        short_model,
        driver: String::new(),
        has_freq,
    }
}

/// Scheduling class of every CPU (CPU sets, Windows 10+), 0 where unknown.
fn sched_classes(n: usize, bases: &[(u16, usize)]) -> Vec<u8> {
    let mut out = vec![0u8; n];
    let mut len = 0u32;
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, std::ptr::null_mut(), 0) };
    let mut b = vec![0u64; (len as usize).div_ceil(8)];
    if len == 0
        || unsafe { GetSystemCpuSetInformation(b.as_mut_ptr() as *mut _, len, &mut len, std::ptr::null_mut(), 0) } == 0
    {
        return out;
    }
    let p = b.as_ptr() as *const u8;
    let mut o = 0usize;
    while o + 8 <= len as usize {
        let r = unsafe { &*(p.add(o) as *const SYSTEM_CPU_SET_INFORMATION) };
        if r.Size == 0 {
            break;
        }
        if r.Type == CpuSetInformation {
            let s = unsafe { &r.Anonymous.CpuSet };
            if let Some(&(_, base)) = bases.iter().find(|g| g.0 == s.Group) {
                let c = base + s.LogicalProcessorIndex as usize;
                if c < n {
                    out[c] = unsafe { s.Anonymous2.SchedulingClass };
                }
            }
        }
        o += r.Size as usize;
    }
    out
}

impl Cpu {
    /// Reads nothing (tests).
    #[cfg(test)]
    pub fn none() -> Cpu {
        Cpu {
            groups: Vec::new(),
            buf: Vec::new(),
            q: std::ptr::null_mut(),
            ctr: std::ptr::null_mut(),
            arr: Vec::new(),
            map: Vec::new(),
            nominal: 0.0,
            la_t: 0,
            lhm_t: 0,
            from_svc: false,
            ceil: f32::MAX,
            read: Vec::new(),
            plan: unsafe { std::mem::zeroed() },
        }
    }

    pub fn new(t: &Topo) -> Cpu {
        let (bases, n) = group_bases();
        let groups = bases
            .iter()
            .enumerate()
            .map(|(k, &(g, b))| (g, b, bases.get(k + 1).map_or(n, |x| x.1) - b))
            .collect();
        let mut c = Cpu {
            groups,
            buf: vec![Sppi::default(); n],
            q: std::ptr::null_mut(),
            ctr: std::ptr::null_mut(),
            arr: Vec::new(),
            map: Vec::new(),
            nominal: nominal_mhz(n),
            la_t: 0,
            lhm_t: 0,
            from_svc: false,
            ceil: if rated_max() > 0.0 { 1.2 * rated_max() } else { f32::MAX },
            read: Vec::new(),
            plan: unsafe { std::mem::zeroed() },
        };
        if t.has_freq {
            unsafe {
                if PdhOpenQueryW(std::ptr::null(), 0, &mut c.q) == ERROR_SUCCESS
                    && PdhAddEnglishCounterW(c.q, wide(PERF).as_ptr(), 0, &mut c.ctr) != ERROR_SUCCESS
                {
                    PdhCloseQuery(c.q);
                    c.q = std::ptr::null_mut();
                }
                if !c.q.is_null() {
                    // (the first collection only starts the interval)
                    PdhCollectQueryData(c.q);
                }
            }
        }
        c
    }

    /// (idle, total) times per CPU, 100 ns units.
    pub fn times(&mut self, cur: &mut [(u64, u64)]) {
        for &(g, base, cnt) in &self.groups {
            let b = &mut self.buf[base..base + cnt];
            let len = std::mem::size_of_val(b) as u32;
            let mut got = 0u32;
            // SystemProcessorPerformanceInformation (8), for group `g`
            let r = unsafe {
                NtQuerySystemInformationEx(8, &g as *const u16 as *const _, 2, b.as_mut_ptr() as *mut _, len, &mut got)
            };
            if r < 0 {
                continue;
            }
            let k = got as usize / std::mem::size_of::<Sppi>();
            for (i, s) in b[..k.min(cnt)].iter().enumerate() {
                if let Some(c) = cur.get_mut(base + i) {
                    *c = (s.idle as u64, (s.kernel + s.user) as u64);
                }
            }
        }
    }

    /// Effective clocks (MHz) of every CPU over the interval since the last call: one
    /// PDH collection for all of them. Idle CPUs show the slowest clock seen (as Linux
    /// reports their minimum). The highest clock of a CPU busy for a quarter of the
    /// interval raises `maxf` (Windows doesn't say how far a CPU boosts).
    ///
    /// A CPU busy for less than a tenth of the interval (the idle thread of a core)
    /// gets a ratio that is noise, 6–9 GHz on a 5.5 GHz CPU: it shows the clock of
    /// its core's other thread (they share it), else its last one (idle: the slowest).
    pub fn clocks(&mut self, ur: &[f32], fr: &mut [f32], t: &mut Topo) {
        if self.q.is_null() || unsafe { PdhCollectQueryData(self.q) } != ERROR_SUCCESS {
            return;
        }
        let (mut size, mut items) = ((self.arr.len() * 8) as u32, 0u32);
        let fmt = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
        let mut r = unsafe {
            PdhGetFormattedCounterArrayW(self.ctr, fmt, &mut size, &mut items, self.arr.as_mut_ptr() as *mut _)
        };
        if r == PDH_MORE_DATA {
            // (sized once: the instance list only changes with CPU hot-add)
            self.arr = vec![0u64; (size as usize).div_ceil(8) + 64];
            size = (self.arr.len() * 8) as u32;
            self.map.clear();
            r = unsafe {
                PdhGetFormattedCounterArrayW(self.ctr, fmt, &mut size, &mut items, self.arr.as_mut_ptr() as *mut _)
            };
        }
        if r != ERROR_SUCCESS {
            return;
        }
        let it = unsafe { std::slice::from_raw_parts(self.arr.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, items as usize) };
        if self.map.len() != it.len() {
            // instance names "group,number" (and "_Total", "0,_Total")
            self.map = it
                .iter()
                .map(|x| {
                    let s = unsafe { crate::win::from_pwstr(x.szName) };
                    let (g, k) = s.split_once(',').unwrap_or(("", ""));
                    match (g.parse::<u16>(), k.parse::<usize>()) {
                        (Ok(g), Ok(k)) => self.groups.iter().find(|x| x.0 == g).map_or(usize::MAX, |x| x.1 + k),
                        _ => usize::MAX,
                    }
                })
                .collect();
        }
        let mut top = 0f32;
        self.read.clear();
        self.read.resize(fr.len(), false);
        for (x, &c) in it.iter().zip(&self.map) {
            if c >= fr.len() || x.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA {
                continue;
            }
            let f = (unsafe { x.FmtValue.Anonymous.doubleValue } as f32 * self.nominal / 100.0).round();
            let u = ur.get(c).copied().unwrap_or(0.0);
            if u <= 0.0 && t.minf > 0.0 {
                fr[c] = t.minf;
                continue;
            }
            if f <= 0.0 || f > self.ceil || u < 0.1 {
                continue;
            }
            if t.minf == 0.0 || f < t.minf {
                t.minf = f;
            }
            if u >= 0.25 {
                top = top.max(f);
            }
            fr[c] = f;
            self.read[c] = true;
        }
        for g in &t.groups {
            if let Some(f) = g.iter().filter(|&&c| self.read[c]).map(|&c| fr[c]).reduce(f32::max) {
                for &c in g.iter().filter(|&&c| !self.read[c]) {
                    fr[c] = f;
                }
            }
        }
        if top > t.maxf {
            t.maxf = (top / 100.0).ceil() * 100.0;
            t.boost = 0.9 * t.maxf;
        }
    }

    /// Load average (1, 5, 15 minutes) every few seconds: the CPUs busy over the
    /// load window, decayed like the kernel's. (Windows' "Processor Queue Length"
    /// would add the threads waiting to run, but reading it walks every thread of
    /// every process: milliseconds.)
    pub fn loadavg(&mut self, busy: f32, now: i64, la: &mut [f32; 3]) {
        // (the first sample of the load window comes after the first call)
        if self.la_t == 0 && busy == 0.0 {
            return;
        }
        if self.la_t == 0 {
            *la = [busy; 3];
        } else {
            let dt = (now - self.la_t) as f32 / 1e9;
            for (v, p) in la.iter_mut().zip([60.0f32, 300.0, 900.0]) {
                let e = (-dt / p).exp();
                *v = *v * e + busy * (1.0 - e);
            }
        }
        self.la_t = now;
    }

    /// The CPU temperatures: from cpuview's sensor service where it is installed (see
    /// win/svc.rs), else from LibreHardwareMonitor's last snapshot; a fresh one is
    /// asked for every 2 s (`all`: smooth) or 30 s (its parse costs a few hundred µs).
    ///
    /// LibreHardwareMonitor may start after cpuview, or close: while there are none,
    /// whether it started is looked at now and then (lhm::look), and the
    /// temperatures come from the helper's first snapshot; they go when it stops
    /// answering.
    pub fn lhm_temps(&mut self, temps: &mut Vec<Sensor>, all: bool) {
        // cpuview's sensor service first (its snapshot, a fresh one asked for: a few
        // µs), the package / Tctl / CCDs as the CPU page shows them
        if let Some(v) = svc::readings() {
            let v: Vec<_> = v.into_iter().filter(|r| !r.minor).collect();
            if !self.from_svc || temps.len() != v.len() || temps.iter().zip(&v).any(|(s, r)| s.label != r.label) {
                *temps = v.iter().map(|r| Sensor { label: r.label.clone(), fd: -1, val: r.val, hist: Ring::new(), max: 0.0 }).collect();
                self.from_svc = true;
            } else {
                for (s, r) in temps.iter_mut().zip(&v) {
                    s.val = r.val;
                }
            }
            return;
        }
        if std::mem::take(&mut self.from_svc) {
            temps.clear();
        }
        let now = crate::term::mono();
        if temps.is_empty() {
            if lhm::live() {
                *temps = lhm::with(cpu_sensors);
            } else {
                lhm::look(now);
            }
            return;
        }
        if !lhm::live() {
            temps.clear();
            return;
        }
        if now - self.lhm_t >= if all { 2 } else { 30 } * 1_000_000_000 {
            self.lhm_t = now;
            lhm::request();
        }
        lhm::with(|v| {
            for s in temps.iter_mut() {
                let x = v.iter().find(|x| x.typ == "Temperature" && x.hwid.contains("cpu") && cpu_temp_label(&x.text).as_deref() == Some(s.label.as_str()));
                if let Some(x) = x.filter(|x| x.val.is_finite()) {
                    s.val = x.val;
                }
            }
        });
    }

    /// The active power plan's name (read again only when the plan changes).
    pub fn policy(&mut self, gov: &mut String) {
        unsafe {
            let mut g: *mut windows_sys::core::GUID = std::ptr::null_mut();
            if PowerGetActiveScheme(std::ptr::null_mut(), &mut g) != ERROR_SUCCESS || g.is_null() {
                return;
            }
            let same = !gov.is_empty() && guid_eq(&*g, &self.plan);
            if !same {
                self.plan = *g;
                let mut b = [0u16; 128];
                let mut n = (b.len() * 2) as u32;
                let r = PowerReadFriendlyName(
                    std::ptr::null_mut(),
                    g,
                    std::ptr::null(),
                    std::ptr::null(),
                    b.as_mut_ptr() as *mut u8,
                    &mut n,
                );
                *gov = if r == ERROR_SUCCESS { from_wide(&b) } else { "?".into() };
            }
            LocalFree(g as *mut _);
        }
    }
}

fn guid_eq(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// Keep the process on one physical core the scheduler uses least, as on Linux: the
/// next wakeup finds its code and data in that core's cache. On hybrid CPUs a
/// performance core (the least preferred of them): an efficiency core makes each
/// wakeup twice as dear (measured on an i9-13900K), against the CPU-time budget.
/// Else the lowest-ranked core, ties the last. Only on single-group machines whose
/// affinity wasn't narrowed already (a narrower one is kept, and pinned inside);
/// CPUVIEW_PIN=off or =N.
pub fn auto_pin(t: &Topo) {
    let env = std::env::var("CPUVIEW_PIN").unwrap_or_default();
    if env == "off" || t.cpus.len() < 4 || unsafe { GetActiveProcessorGroupCount() } > 1 {
        return;
    }
    let (mut cur, mut sys) = (0usize, 0usize);
    if unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut cur, &mut sys) } == 0 {
        return;
    }
    let bit = |c: usize| if c < usize::BITS as usize { 1usize << c } else { 0 };
    let allowed = |g: &Vec<usize>| g.iter().any(|&i| cur & bit(t.cpus[i]) != 0);
    let mut set = 0usize;
    if let Ok(n) = env.parse::<usize>() {
        set = bit(n);
    } else {
        let ok: Vec<usize> = (0..t.groups.len()).filter(|&g| allowed(&t.groups[g])).collect();
        if ok.len() < 2 {
            return;
        }
        // (rank: efficiency class << 8 | scheduling class; ties: the last of them)
        let top = ok.iter().map(|&g| t.rank[g] >> 8).max().unwrap_or(-1);
        let key = |g: usize| (t.rank[g].max(-1), -(g as i64));
        let Some(&g) = ok.iter().filter(|&&g| t.rank[g] >> 8 == top).min_by_key(|&&g| key(g)) else {
            return;
        };
        for &i in &t.groups[g] {
            set |= cur & bit(t.cpus[i]);
        }
    }
    if set != 0 {
        unsafe { SetProcessAffinityMask(GetCurrentProcess(), set) };
    }
}

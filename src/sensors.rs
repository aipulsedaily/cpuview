//! View 5 data: fans, temperatures, voltages and power, all auto-detected (hwmon,
//! thermal zones, RAPL through powercap or perf, power_supply), with plain names for
//! the cryptic sensor labels; and `cpuview --setup-sensors`.
//!
//! Read costs differ by three orders of magnitude (Ryzen 9 9950X, ASUS B850): a
//! k10temp temperature 3–6 µs, a cached hwmon attribute 1–2 µs, but a motherboard
//! chip of the nct6775 family re-reads its ~300 registers over the LPC bus when its
//! 1.5 s cache is stale: 2.5 ms of CPU for the first attribute. DIMM sensors (SMBus)
//! cost ~95 µs each, the amdgpu power reading (a message to the SMU) 50–400 µs. So
//! every source is read on its own cadence, stretched by its measured cost to a share
//! of the CPU budget (`period`), and nothing is read while the page is hidden.

use crate::sys::{isort, pread, Ring};
use std::os::raw::c_int;

pub const SEC: i64 = 1_000_000_000;

// what a reading belongs to
pub const G_CPU: u8 = 0;
pub const G_BOARD: u8 = 1;
pub const G_MEM: u8 = 2;
pub const G_DISK: u8 = 3;
pub const G_GPU: u8 = 4;
pub const G_NET: u8 = 5;
pub const G_OTHER: u8 = 6;
/// A group's name.
pub fn group_name(g: u8) -> &'static str {
    match g {
        G_CPU => "CPU",
        G_BOARD => "motherboard",
        G_MEM => "memory",
        G_DISK => "storage",
        G_GPU => "graphics",
        G_NET => "network",
        _ => "other",
    }
}

// kinds of reading
pub const K_FAN: u8 = 0;
pub const K_TEMP: u8 = 1;
pub const K_VOLT: u8 = 2;
pub const K_POWER: u8 = 3;

// power readings
pub const R_PKG: u8 = 1;
/// Intel's "core" RAPL domain: all cores together
pub const R_CORES: u8 = 2;
/// one core (AMD, perf power_core)
pub const R_CORE: u8 = 3;
pub const R_UNCORE: u8 = 4;
pub const R_DRAM: u8 = 5;
pub const R_PSYS: u8 = 6;
pub const R_GPU: u8 = 7;
pub const R_BAT: u8 = 8;
pub const R_OTHER: u8 = 9;

// sources
const S_HWMON: u8 = 0;
const S_ZONES: u8 = 1;
const S_RAPL: u8 = 2;
const S_PERF: u8 = 3;
const S_BAT: u8 = 4;
const S_BMC: u8 = 5;

// cadence classes (CPU, board, GPU, slow, power, cores, battery: `CHIPS` names
// them for hwmon chips): (eco base ms, base ms, eco share µs/s, share µs/s). A
// source is read every base, or less often if a read costs more than the share
// allows.
const C_CPU: usize = 0;
const C_SLOW: usize = 3;
/// eco on the overview: a dear source (> 0.2 ms a read) at most this often
const OV_DEAR: i64 = 600 * SEC;
const C_POWER: usize = 4;
const C_CORES: usize = 5;
const C_BAT: usize = 6;
const CADENCE: [(i64, i64, i64, i64); 7] = [
    (3000, 500, 5, 300),
    (5000, 2000, 38, 600),
    (10000, 1000, 3, 300),
    (60000, 3000, 2, 200),
    (2000, 500, 6, 500),
    (5000, 1000, 4, 500),
    (30000, 5000, 2, 200),
];

// CPU power availability
pub const P_OK: u8 = 0;
/// counters exist but are root-only (RAPL energy_uj 0400, perf_event_paranoid)
pub const P_LOCKED: u8 = 1;
pub const P_NONE: u8 = 2;

// temperature flags (`temp_name`)
const F_MINOR: u8 = 1;
/// an unidentified motherboard input: dropped when implausibly cold
const F_AUX: u8 = 2;
const F_DROP: u8 = 4;
/// the motherboard's own temperature (reference for the aux inputs)
const F_REF: u8 = 8;
/// a BMC reading asked for, its answer not in yet
const F_ASKED: u8 = 16;

/// An energy counter: watts from the difference of two readings.
#[derive(Clone, Copy, Default)]
pub struct Ctr {
    perf: bool,
    /// powercap: the counter wraps after this many µJ (max_energy_range_uj)
    pub range: u64,
    /// joules per count
    pub scale: f64,
    prev: u64,
    prev_t: i64,
    have: bool,
}

/// Counter increase from `prev` to `cur` for a counter that wraps to 0 after `range`
/// (0: never wraps, e.g. perf's 64-bit counts).
pub fn wrap_delta(prev: u64, cur: u64, range: u64) -> u64 {
    if cur >= prev {
        cur - prev
    } else if range > prev {
        cur + (range - prev)
    } else {
        0
    }
}

impl Ctr {
    /// Watts since the previous reading. RAPL counters advance about once a
    /// millisecond (AMD and Intel alike), so a window of a few ms could be off by a
    /// whole update; windows are at least 0.4 s (shorter ones keep the older start),
    /// which keeps that error under 0.3 %. None for the first reading.
    pub fn update(&mut self, v: u64, t: i64) -> Option<f32> {
        if !self.have {
            (self.have, self.prev, self.prev_t) = (true, v, t);
            return None;
        }
        let dt = t - self.prev_t;
        if dt < 400_000_000 {
            return None;
        }
        let d = wrap_delta(self.prev, v, self.range);
        (self.prev, self.prev_t) = (v, t);
        Some((d as f64 * self.scale * 1e9 / dt as f64) as f32)
    }
}

/// A source: one hwmon chip, the thermal zones, the RAPL counters or the batteries.
pub struct Src {
    /// hwmon name ("nct6799"), or what it is
    pub chip: String,
    /// kernel driver ("nct6775")
    pub driver: String,
    /// in plain words
    pub what: String,
    /// where it sits (bus address, interface)
    pub dev: String,
    kind: u8,
    class: usize,
    pub group: u8,
    /// CPU time of a read (ns): a slowly decaying peak
    pub cost: i64,
    pub last: i64,
    reads: u32,
    /// fans, temperatures, voltages, power readings found
    pub n: [u16; 4],
}

/// One reading: a fan, a temperature, a voltage or a power.
pub struct Rd {
    pub kind: u8,
    pub src: u8,
    pub group: u8,
    pub role: u8,
    fd: c_int,
    /// the name the hardware gives it ("AUXTIN3", "fan2", "Tctl")
    pub raw: String,
    /// in plain words ("CPU fan", "motherboard", "RAM stick 1")
    pub name: String,
    /// °C, RPM, V, W
    pub val: f32,
    pub ok: bool,
    pub seen: bool,
    /// temperatures: amber and red from; voltages: limits (0: none)
    pub warn: f32,
    pub crit: f32,
    pub minor: bool,
    /// an unidentified input (temperatures) / a header name that's a guess (fans)
    pub guess: bool,
    flags: u8,
    mul: f32,
    pub hist: Option<Box<Ring<f32>>>,
    /// highest value of the last 2 minutes
    pub peak: f32,
    /// fans: highest speed seen
    pub max: f32,
    pwm_fd: c_int,
    en_fd: c_int,
    mode_fd: c_int,
    /// fans: duty 0..1 (-1: not readable), control mode (-1: unknown), 3-pin (1) or
    /// 4-pin (0), -1 unknown
    pub pwm: f32,
    pub en: i8,
    pub dc: i8,
    pub ctr: Option<Ctr>,
}

fn rd(kind: u8, src: usize, group: u8, fd: c_int, raw: &str, name: &str) -> Rd {
    Rd {
        kind,
        src: src as u8,
        group,
        role: 0,
        fd,
        raw: raw.to_string(),
        name: name.to_string(),
        val: 0.0,
        ok: false,
        seen: false,
        warn: 0.0,
        crit: 0.0,
        minor: false,
        guess: false,
        flags: 0,
        mul: 1.0,
        hist: None,
        peak: 0.0,
        max: 0.0,
        pwm_fd: -1,
        en_fd: -1,
        mode_fd: -1,
        pwm: -1.0,
        en: -1,
        dc: -1,
        ctr: None,
    }
}

/// A battery (laptops, UPS-less desktops have none).
pub struct Bat {
    pub name: String,
    fds: [c_int; 7],
    /// charge_* (µAh) instead of energy_* (µWh)
    charge: bool,
    pub pct: f32,
    pub w: f32,
    /// 0 unknown, 1 charging, 2 discharging, 3 full, 4 not charging
    pub st: u8,
    /// seconds to empty (discharging) or full (charging), 0 unknown
    pub left: f32,
    /// its power reading in `rd`
    pub rd: usize,
}
const B_CAP: usize = 0;
const B_STATUS: usize = 1;
const B_PNOW: usize = 2;
const B_CNOW: usize = 3;
const B_VNOW: usize = 4;
const B_ENOW: usize = 5;
const B_EFULL: usize = 6;

pub struct Sens {
    root: String,
    pub detected: bool,
    pub srcs: Vec<Src>,
    pub rd: Vec<Rd>,
    pub bats: Vec<Bat>,
    ac: c_int,
    /// mains power: 1 plugged in, 0 not, -1 unknown / none
    pub ac_on: i8,
    /// "ASUSTeK COMPUTER INC. ProArt B850-CREATOR WIFI NEO"
    pub board: String,
    pub vm: bool,
    amd: bool,
    /// no fans found: the driver that would likely show them, and whether it is
    /// available (0), already loaded (1) or unknown (2: run sensors-detect); or 3: the
    /// board's BMC runs the fans (IPMI), which no hwmon driver shows
    pub fan_hint: Option<(String, u8)>,
    pub cpu_power: u8,
    /// RAPL exists but is root-only; perf energy counters exist but are denied
    pub rapl_locked: bool,
    pub perf_locked: bool,
    /// history points pushed
    pub pts: u64,
    last_seen: i64,
    /// CPU time spent reading since `take_spent`
    spent: i64,
    pub perf_paranoid: i32,
    /// the overview's last dear read (not counting the one when it came into view)
    ov_dear: i64,
    /// a server board's BMC (IPMI) and its sensors (a BMC reading's `fd` is its index)
    bmc: Option<crate::ipmi::Bmc>,
    sdr: Vec<crate::ipmi::Sdr>,
    /// there is a BMC but its device is root-only
    pub bmc_locked: bool,
}

fn thread_ns() -> i64 {
    crate::term::now_ns(libc::CLOCK_THREAD_CPUTIME_ID)
}


/// Leading signed integer.
pub fn parse_i64(b: &[u8]) -> Option<i64> {
    let mut i = 0;
    while i < b.len() && b[i] == b' ' {
        i += 1;
    }
    let neg = i < b.len() && b[i] == b'-';
    i += neg as usize;
    let s = i;
    let mut v: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v.wrapping_mul(10).wrapping_add((b[i] - b'0') as i64);
        i += 1;
    }
    (i > s).then_some(if neg { -v } else { v })
}

fn read_fd(fd: c_int) -> Option<i64> {
    let mut buf = [0u8; 24];
    let n = pread(fd, &mut buf);
    parse_i64(&buf[..n])
}

fn num_of(s: &str) -> Option<u32> {
    let mut v = None;
    for b in s.bytes() {
        if b.is_ascii_digit() {
            v = Some(v.unwrap_or(0u32).wrapping_mul(10) + (b - b'0') as u32);
        }
    }
    v
}

/// Directory entries (names), "fan2" before "fan10".
fn list(d: &str) -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir(d) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect(),
        Err(_) => Vec::new(),
    };
    isort(&mut v, &|a: &String, b: &String| nat_lt(a, b));
    v
}

/// Natural order: digit runs compare as numbers.
fn nat_lt(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut k) = (0, 0);
    while i < a.len() && k < b.len() {
        if a[i].is_ascii_digit() && b[k].is_ascii_digit() {
            let (mut x, mut y) = (0u64, 0u64);
            while i < a.len() && a[i].is_ascii_digit() {
                x = x * 10 + (a[i] - b'0') as u64;
                i += 1;
            }
            while k < b.len() && b[k].is_ascii_digit() {
                y = y * 10 + (b[k] - b'0') as u64;
                k += 1;
            }
            if x != y {
                return x < y;
            }
        } else {
            if a[i] != b[k] {
                return a[i] < b[k];
            }
            i += 1;
            k += 1;
        }
    }
    a.len() - i < b.len() - k
}


fn superio(chip: &str) -> bool {
    ["nct", "it8", "w83", "f71", "f81", "sch5", "asus", "dell", "gigabyte", "hp"]
        .iter()
        .any(|p| chip.starts_with(p))
        || matches!(chip, "thinkpad" | "applesmc")
}

/// What hwmon chips are: "chips|*|group|cadence class|in plain words" (see
/// `lookup`).
const CHIPS: &str = "\
k10temp,zenpower|*|0|0|CPU temperatures (AMD)
coretemp|*|0|0|CPU temperatures (Intel)
spd5118,jc42,ee1004|*|2|3|RAM stick temperature
nvme|*|3|3|SSD temperatures
drivetemp|*|3|3|drive temperature
amdgpu,radeon,nouveau,i915,xe|*|4|2|graphics
acpitz|*|6|3|firmware (ACPI) thermal zone
dell|*|1|1|Dell fans & temperatures
thinkpad|*|1|1|ThinkPad fans & temperatures
applesmc|*|1|1|Apple SMC fans & temperatures
asus|*|1|1|ASUS motherboard sensors
@|*|1|1|motherboard chip
iwlwifi|*|5|3|Wi-Fi card
pch_|*|1|3|chipset
cpu_thermal,cpu-thermal,cpu0,soc_thermal,soc-thermal|*|0|0|CPU temperature (SoC)
gpu_thermal,gpu-thermal|*|4|2|GPU temperature
|*|6|3|";

/// Temperatures: "chips|labels|plain name|group|flags" (see `lookup`; thermal zones
/// are chip "zone" with their type as the label).
const TEMPS: &str = "\
k10temp,zenpower|tctl|CPU|0|0
k10temp,zenpower|tdie|CPU die|0|0
k10temp,zenpower|tccd*|CPU chiplet #|0|0
k10temp,zenpower|*|@|0|1
coretemp|core*|core #|0|1
coretemp|package id 0|CPU|0|0
coretemp|package*|CPU #|0|0
coretemp|*|CPU|0|0
nvme|sensor*|SSD sensor #|3|1
nvme|*|SSD|3|0
amdgpu,radeon,nouveau,i915,xe|junction|GPU hotspot|4|1
amdgpu,radeon,nouveau,i915,xe|mem,vram|GPU memory|4|1
amdgpu,radeon,nouveau,i915,xe|*|GPU|4|0
spd5118,jc42,ee1004|*|RAM stick|2|0
drivetemp|*|drive|3|0
iwlwifi|*|Wi-Fi|5|0
cpu_thermal,cpu-thermal,cpu0_thermal,cpu0-thermal|*|CPU|0|0
soc_thermal,soc-thermal|*|chip (SoC)|0|0
gpu_thermal,gpu-thermal|*|GPU|4|0
acpitz|*|ACPI zone #|6|1
pch_|*|chipset|1|0
zone|x86_pkg_temp|CPU package|0|0
zone|int3400*|@|6|4
zone|~cpu,tcpu,b0d4|CPU|0|0
zone|~soc|chip (SoC)|0|0
zone|~gpu|GPU|4|0
zone|pch*|chipset|1|0
zone|iwlwifi*|Wi-Fi|5|0
zone|~ddr,~mem|memory|2|0
zone|~nvme,~ssd|SSD|3|0
zone|tskn,~skin|laptop surface|6|0
zone|acpitz|ACPI zone|6|1
zone|*|@|6|0
@||input #|1|2
@|systin,motherboard,system,ambient,mb|motherboard|1|8
@|cputin|CPU socket (board)|1|0
@|chipset,pch_chip_temp,pch|chipset|1|0
@|pch_cpu_temp|CPU (via chipset)|0|1
@|pch_chip_cpu_max_temp|chipset (CPU max)|1|1
@|pch_mch_temp|memory controller|1|1
@|vrm|CPU power stage (VRM)|1|0
@|t_sensor,t_sensor1|extra probe (T_SENSOR)|6|0
@|water_in|coolant in|6|0
@|water_out|coolant out|6|0
@|~calibration|@|0|4
@|auxtin*|aux input #|1|2
@|tsi*,peci*,smbusmaster*,~cpu|CPU (board's reading)|0|1
@|~gpu,~video|GPU (board's reading)|4|1
@|~dimm,~memory,~ram|RAM (board's reading)|2|1
@|~ssd,~m.2,~m2,~disk|SSD (board's reading)|3|1
@|*|@|1|2
||$ #|6|0
|*|@|6|0";

/// The fields after the first line of `t` ("chips|labels|a|b|c") that matches:
/// chips are comma-separated name prefixes ("@": a motherboard chip, empty: any);
/// labels comma-separated, lower case: "x" exactly, "x*" starting with, "~x"
/// containing, "*" anything ("" matches an empty label only).
fn lookup<'a>(t: &'a str, chip: &str, lab: &str) -> [&'a str; 3] {
    let l = lab.to_ascii_lowercase();
    for line in t.lines() {
        let mut f = line.split('|');
        let (c, ls) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
        let cm = c.is_empty()
            || c.split(',').any(|p| if p == "@" { superio(chip) } else { chip.starts_with(p) });
        let lm = ls.split(',').any(|p| match (p.strip_prefix('~'), p.strip_suffix('*')) {
            (Some(w), _) => l.contains(w),
            (_, Some(pre)) => l.starts_with(pre),
            _ => p == l,
        });
        if cm && lm {
            return [f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or("")];
        }
    }
    ["", "", ""]
}

/// A small table number.
fn tn(s: &str) -> u8 {
    s.bytes().fold(0, |a, b| a * 10 + b.wrapping_sub(b'0'))
}

/// A table name filled in: "#" the number, "@" the label, "$" the chip.
fn expand(t: &str, lab: &str, chip: &str, k: u32) -> String {
    let mut s = String::new();
    for c in t.chars() {
        match c {
            '#' => s += &n2s(k),
            '@' => s += lab,
            '$' => s += chip,
            c => s.push(c),
        }
    }
    s
}

/// A number as a string.
fn n2s(n: u32) -> String {
    let mut f = crate::ui::F::new();
    f.int(n, 0);
    f.s().to_string()
}

/// A hwmon chip: (group, what it is in plain words, cadence class).
fn chip_info(chip: &str) -> (u8, &'static str, usize) {
    let [g, c, w] = lookup(CHIPS, chip, "");
    (tn(g), w, tn(c) as usize)
}

/// Plain name, group and flags of a temperature: chip, its label (may be empty) and
/// number.
pub fn temp_name(chip: &str, lab: &str, n: u32) -> (String, u8, u8) {
    let [name, g, fl] = lookup(TEMPS, chip, lab);
    (expand(name, lab, chip, num_of(lab).unwrap_or(n)), tn(g), tn(fl))
}

/// Fans: "chips|label|plain name|header is a guess|header" (`lookup`; unlabelled
/// fans are looked up as "fanN", Nuvoton chips on ASUS boards as "asus:nct…").
/// Nuvoton's FANIN1 is the system fan pin, FANIN2 the CPU fan pin; ASUS wires them
/// CHA_FAN1, CPU_FAN, CHA_FAN2, CHA_FAN3, CPU_OPT, AIO_PUMP, W_PUMP+ (CPU_FAN and
/// CPU_OPT are the same on every recent ASUS board).
const FANS: &str = "\
asus:nct|fan1|case fan 1|1|CHA_FAN1?
asus:nct|fan2|CPU fan|0|CPU_FAN
asus:nct|fan3|case fan 2|1|CHA_FAN2?
asus:nct|fan4|case fan 3|1|CHA_FAN3?
asus:nct|fan5|CPU fan 2|0|CPU_OPT
asus:nct|fan6|pump|1|AIO_PUMP?
asus:nct|fan7|pump 2|1|W_PUMP+?
nct|fan2|fan 2|1|CPU_FAN?
amdgpu,radeon,nouveau|fan*|GPU fan|0|
thinkpad,dell_smm,applesmc|fan1|fan|0|
|fan*|fan #|1|
|~cpu_opt,~cpu opt,~cpu2,~cpu 2|CPU fan 2|0|@
|~cpu,~processor|CPU fan|0|@
|~pump|pump|0|@
|~gpu,~video,~graphics|GPU fan|0|@
|~cha,~sys,~case,~chassis|case fan|0|@
|~psu,~power supply|power supply fan|0|@
|*|@|0|@";

/// Fan: (plain name, header / raw name, the header is a guess).
pub fn fan_name(chip: &str, lab: &str, n: u32, asus: bool) -> (String, String, bool) {
    let raw = String::from("fan") + &n2s(n);
    let l = if lab.is_empty() { raw.as_str() } else { lab };
    let c = if asus && chip.starts_with("nct") { String::from("asus:") + chip } else { chip.to_string() };
    let [name, g, h] = lookup(FANS, &c, l);
    let r = if h.is_empty() { raw.clone() } else { expand(h, l, chip, n) + " · " + &raw };
    (expand(name, l, chip, n), r, g == "1")
}

/// Voltages: "chips|label|plain name|multiplier" (`lookup`; unlabelled inputs are
/// looked up as "inN"): Nuvoton's usual inputs, and the +5 V / +12 V rails through
/// their usual dividers (used only where that gives a plausible value).
const VOLTS: &str = "\
nct|in0|CPU core (Vcore)|1
nct|in1|+5 V|5
nct|in2|3.3 V analog|1
nct|in3|+3.3 V|1
nct|in4|+12 V|12
nct|in7|3.3 V standby|1
nct|in8|CMOS battery|1
|vcore,cpu core,cpuvcore|CPU core (Vcore)|1
|+12v,12v|+12 V|1
|+5v,5v|+5 V|1
|3vcc,+3.3v,3.3v|+3.3 V|1
|3vsb|3.3 V standby|1
|vbat,vbatt|CMOS battery|1
|avcc|3.3 V analog|1
|vddgfx|GPU core|1
|vddnb|SoC (northbridge)|1
|vddio,vddio_mem|memory I/O (VDDIO)|1
|*|@|1";

/// Voltage: (plain name, multiplier of the reading).
pub fn volt_name(chip: &str, lab: &str, n: u32, v: f32) -> (String, f32) {
    let raw = String::from("in") + &n2s(n);
    let l = if lab.is_empty() { raw.as_str() } else { lab };
    let [name, m, _] = lookup(VOLTS, chip, l);
    let m = tn(m) as f32;
    if m > 1.0 && (v * m - m).abs() > m * 0.12 {
        return (raw, 1.0);
    }
    (expand(name, l, chip, n), m)
}

/// Default (amber, red) temperatures per group, for sensors that give no limits.
fn temp_defaults(g: u8) -> (f32, f32) {
    match g {
        G_CPU => (85.0, 95.0),
        G_BOARD => (70.0, 90.0),
        G_MEM => (70.0, 85.0),
        G_DISK => (70.0, 80.0),
        G_GPU => (85.0, 100.0),
        G_NET => (90.0, 110.0),
        _ => (80.0, 100.0),
    }
}

/// A limit in millidegrees → °C if plausible (chips report 0, 65261 °C, …).
fn sane_limit(v: Option<i64>) -> Option<f32> {
    let c = v? as f32 / 1000.0;
    (40.0..=150.0).contains(&c).then_some(c)
}

/// (amber, red) from the chip's max / crit and the group's defaults.
pub fn temp_limits(g: u8, max: Option<i64>, crit: Option<i64>) -> (f32, f32) {
    let (dw, dc) = temp_defaults(g);
    let (m, c) = (sane_limit(max), sane_limit(crit));
    let crit = c.or(m).unwrap_or(dc);
    let warn = match (m, c) {
        (Some(m), Some(c)) if m < c => m.max(dw).min(c - 1.0),
        _ => dw.min(crit - 5.0),
    };
    (warn, crit)
}

/// Is a temperature reading real? (placeholders read 0, unconnected inputs −60,
/// −128 or 127; an unidentified board input colder than the board itself by more
/// than 8 °C is a floating pin: nothing inside a running computer is.)
fn temp_ok(v: f32, aux: bool, board: Option<f32>) -> bool {
    if v == 0.0 || v <= -40.0 || v >= 127.0 {
        return false;
    }
    !aux || (v >= 10.0 && board.is_none_or(|b| v >= b - 8.0))
}

/// perf_event_open for a system-wide counter on `cpu`; -errno on failure.
fn perf_open(typ: u32, config: u64, cpu: i32) -> c_int {
    let mut attr = [0u64; 16];
    attr[0] = typ as u64 | (128u64 << 32);
    attr[1] = config;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            attr.as_ptr(),
            -1 as libc::pid_t,
            cpu,
            -1 as c_int,
            8 as libc::c_ulong, // PERF_FLAG_FD_CLOEXEC
        )
    };
    if fd < 0 {
        -std::io::Error::last_os_error().raw_os_error().unwrap_or(1)
    } else {
        fd as c_int
    }
}

/// "event=0x02" → 2.
fn perf_event_cfg(s: &str) -> Option<u64> {
    let v = s.trim().strip_prefix("event=")?;
    if v.contains(',') {
        return None;
    }
    let v = v.strip_prefix("0x").unwrap_or(v);
    let mut r = 0u64;
    for c in v.bytes() {
        r = r * 16 + (c as char).to_digit(16)? as u64;
    }
    Some(r)
}

/// "2.3283064365386962890625e-10" (no float parser in the binary).
fn parse_sci(s: &str) -> Option<f64> {
    let s = s.trim();
    let (m, e) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, parse_i64(e.as_bytes())? as i32),
        None => (s, 0),
    };
    // mantissa digits as f64 (pf works in f32: not enough digits for a scale)
    let (neg, m) = match m.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, m),
    };
    let (mut v, mut sc, mut frac) = (0f64, 1f64, false);
    for b in m.bytes() {
        match b {
            b'0'..=b'9' => {
                v = v * 10.0 + (b - b'0') as f64;
                if frac {
                    sc *= 10.0;
                }
            }
            b'.' if !frac => frac = true,
            _ => return None,
        }
    }
    let r = v / sc * 10f64.powi(e);
    Some(if neg { -r } else { r })
}


/// A path built on the stack (no allocation): the parts joined.
pub struct Pb {
    b: [u8; 400],
    n: usize,
}

impl Pb {
    fn s(&self) -> &str {
        std::str::from_utf8(&self.b[..self.n]).unwrap_or("")
    }
}

#[inline(never)]
fn pb(parts: &[&str]) -> Pb {
    let mut p = Pb { b: [0; 400], n: 0 };
    for s in parts {
        let k = s.len().min(399 - p.n);
        p.b[p.n..p.n + k].copy_from_slice(&s.as_bytes()[..k]);
        p.n += k;
    }
    p
}

/// open(2) read-only (`p` NUL-terminated on the stack).
fn openp(p: &Pb) -> c_int {
    unsafe { libc::open(p.b.as_ptr() as *const _, libc::O_RDONLY | libc::O_CLOEXEC) }
}

/// A small file's text, trimmed (in `b`).
fn slurp<'a>(p: &Pb, b: &'a mut [u8; 256]) -> &'a str {
    let fd = openp(p);
    let n = pread(fd, b);
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
    std::str::from_utf8(&b[..n]).unwrap_or("").trim()
}

/// A small file's text, trimmed.
#[inline(never)]
fn rstr(parts: &[&str]) -> String {
    let mut b = [0u8; 256];
    slurp(&pb(parts), &mut b).to_string()
}

/// A file's leading integer.
#[inline(never)]
fn rint(parts: &[&str]) -> Option<i64> {
    let mut b = [0u8; 256];
    parse_i64(slurp(&pb(parts), &mut b).as_bytes())
}

#[inline(never)]
fn open_at(parts: &[&str]) -> c_int {
    openp(&pb(parts))
}

#[inline(never)]
fn exists(parts: &[&str]) -> bool {
    unsafe { libc::access(pb(parts).b.as_ptr() as *const _, libc::F_OK) == 0 }
}

/// Where the BMC's sensor list is cached (walking it takes the BMC a while).
fn bmc_cache() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    base.join("cpuview").join("bmc-sdr")
}

/// The network interface a device belongs to (a `net` directory on its way up, short of
/// /sys/devices/virtual, whose net/ holds every virtual interface: lo, bridges, …).
fn iface_of(dev: &str, root: &str) -> Option<String> {
    let mut p = dev;
    while p.len() > root.len() + 5 && !p.ends_with("/devices/virtual") {
        if let Some(n) = list(&[p, "/net"].concat()).into_iter().next() {
            return Some(n);
        }
        p = &p[..p.rfind('/')?];
    }
    None
}

/// Kernel release (`uname -r`).
fn release() -> String {
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        libc::uname(&mut u);
        std::ffi::CStr::from_ptr(u.release.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Drivers that likely show a board's fans: "vendor (and product) prefix|*|modules"
/// (`lookup` on the lower-case DMI vendor and product version).
const MODS: &str = "\
asus|*|nct6775,asus_ec_sensors,asus_wmi_sensors
micro-star,msi|*|nct6775,nct6683
gigabyte|*|it87,gigabyte_wmi
asrock|*|nct6775,nct6683
dell|*|dell_smm_hwmon
lenovo thinkpad|*|thinkpad_acpi
apple|*|applesmc
hp,hewlett|*|hp_wmi_sensors
supermicro|*|nct6775";

/// `s` with byte `a` replaced by `b` (both ASCII).
fn us_dash(s: &str, a: u8, b: u8) -> String {
    s.bytes().map(|c| (if c == a { b } else { c }) as char).collect()
}

/// Is kernel module `m` installed (no modinfo: the usual places)?
fn module_file(root: &str, m: &str) -> bool {
    let base = [root, "/lib/modules/", &release(), "/kernel/drivers/"].concat();
    let dash = us_dash(m, b'_', b'-');
    for dir in "hwmon/ platform/x86/ platform/x86/dell/ platform/x86/lenovo/".split(' ') {
        for name in [m, &dash] {
            for ext in ".ko .ko.zst .ko.xz .ko.gz".split(' ') {
                if exists(&[&base, dir, name, ext]) {
                    return true;
                }
            }
        }
    }
    false
}

impl Sens {
    pub fn empty() -> Sens {
        Sens::at("")
    }

    /// Sensors under `root` ("" for the real /sys; tests use a fake tree).
    pub fn at(root: &str) -> Sens {
        Sens {
            root: root.to_string(),
            detected: false,
            srcs: Vec::new(),
            rd: Vec::new(),
            bats: Vec::new(),
            ac: -1,
            ac_on: -1,
            board: String::new(),
            vm: false,
            amd: false,
            fan_hint: None,
            cpu_power: P_NONE,
            rapl_locked: false,
            perf_locked: false,
            pts: 0,
            last_seen: i64::MIN,
            spent: 0,
            perf_paranoid: -9,
            ov_dear: i64::MIN / 2,
            bmc: None,
            sdr: Vec::new(),
            bmc_locked: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn src(&mut self, chip: &str, driver: &str, what: &str, dev: &str, kind: u8, class: usize, group: u8) -> usize {
        self.srcs.push(Src {
            chip: chip.to_string(),
            driver: driver.to_string(),
            what: what.to_string(),
            dev: dev.to_string(),
            kind,
            class,
            group,
            cost: 0,
            last: i64::MIN,
            reads: 0,
            n: [0; 4],
        });
        self.srcs.len() - 1
    }

    /// Add reading `r` of source `s` (if its file opened).
    fn add(&mut self, s: usize, r: Rd) {
        if r.fd >= 0 || r.role == R_BAT {
            self.srcs[s].n[r.kind as usize] += 1;
            self.rd.push(r);
        }
    }

    /// Find everything, once (a driver loaded later shows after a restart), and
    /// read it.
    pub fn detect(&mut self, now: i64) {
        self.detected = true;
        let root = self.root.clone();
        let root = root.as_str();
        let dmi = |f: &str| rstr(&[root, "/sys/class/dmi/id/", f]);
        let (bv, bn, sv, pn) = (dmi("board_vendor"), dmi("board_name"), dmi("sys_vendor"), dmi("product_name"));
        let mut vendor = if bv.is_empty() { sv.clone() } else { bv };
        for (a, b) in "ASUSTeK COMPUTER INC.=ASUS|Micro-Star International Co., Ltd.=MSI|Gigabyte Technology Co., Ltd.=Gigabyte|Dell Inc.=Dell".split('|').filter_map(|p| p.split_once('=')) {
            if vendor == a {
                vendor = b.to_string();
            }
        }
        let model = if !bn.is_empty() && !bn.starts_with("To be filled") { &bn } else { &pn };
        self.board = if model.is_empty() {
            rstr(&[root, "/proc/device-tree/model"]).trim_end_matches('\0').to_string()
        } else {
            [&vendor, " ", model].concat()
        };
        let vs = [&sv, " ", &pn].concat().to_ascii_lowercase();
        self.vm = "qemu|kvm|vmware|virtualbox|innotek|xen|bochs|parallels|virtual machine".split('|').any(|w| vs.contains(w))
            || exists(&[root, "/sys/hypervisor/type"]);
        let cpu = std::fs::read(pb(&[root, "/proc/cpuinfo"]).s()).unwrap_or_default();
        self.amd = cpu.windows(12).any(|w| w == b"AuthenticAMD" || w == b"HygonGenuine");
        // hwmon chips
        let hw = [root, "/sys/class/hwmon/"].concat();
        let mut names = Vec::new();
        for d in list(&hw) {
            let d = [&hw, d.as_str()].concat();
            let name = rstr(&[&d, "/name"]);
            // (finding a chip reads it: an expensive chip shows its cost here first)
            let t0 = thread_ns();
            self.hwmon(&d, &name, vendor.starts_with("ASUS"));
            let c = thread_ns() - t0;
            if let (Some(s), true) = (self.srcs.last_mut(), c > 1_000_000) {
                s.cost = c;
            }
            names.push(us_dash(&name, b'-', b'_'));
        }
        // thermal zones the hwmon chips don't already show
        let tz = [root, "/sys/class/thermal/"].concat();
        for d in list(&tz) {
            let t = rstr(&[&tz, &d, "/type"]);
            let (name, g, fl) = temp_name("zone", &t, 0);
            let dup = names.contains(&us_dash(&t, b'-', b'_')) || (t == "x86_pkg_temp" && names.iter().any(|n| n == "coretemp"));
            if dup || fl & F_DROP != 0 {
                continue;
            }
            let fd = open_at(&[&tz, &d, "/temp"]);
            if fd < 0 {
                continue;
            }
            let s = match self.srcs.iter().position(|s| s.kind == S_ZONES) {
                Some(s) => s,
                None => self.src("thermal zones", "thermal", "kernel thermal zones", "", S_ZONES, C_CPU, G_CPU),
            };
            let mut r = rd(K_TEMP, s, g, fd, &t, &name);
            r.minor = fl & F_MINOR != 0;
            (r.warn, r.crit) = temp_limits(g, None, rint(&[&tz, &d, "/trip_point_0_temp"]));
            self.add(s, r);
        }
        self.rapl();
        self.batteries();
        let has_bmc = ["/sys/class/ipmi/ipmi0", "/dev/ipmi0", "/dev/ipmi/0", "/dev/ipmidev/0"].iter().any(|p| exists(&[root, p]));
        if has_bmc {
            match crate::ipmi::Bmc::open(root) {
                Some(b) => {
                    let sdr = b.sensors(&bmc_cache());
                    self.bmc = Some(b);
                    self.add_bmc(sdr);
                }
                None => self.bmc_locked = true,
            }
        }
        self.finish_names();
        // no fans: which driver would show them (by the board's maker), unless a BMC
        // (IPMI) runs them, as on server boards
        if !self.rd.iter().any(|r| r.kind == K_FAN) && !self.vm && !vendor.is_empty() {
            let key = [&vendor, " ", &dmi("product_version")].concat().to_ascii_lowercase();
            let mut hint = (String::new(), 2);
            if has_bmc {
                hint = ("ipmi".to_string(), 3);
            }
            for m in lookup(MODS, &key, "")[0].split(',').filter(|m| !has_bmc && !m.is_empty()) {
                if exists(&[root, "/sys/module/", m]) {
                    if hint.1 == 2 {
                        hint = (m.to_string(), 1);
                    }
                } else if module_file(root, m) {
                    hint = (m.to_string(), 0);
                    break;
                }
            }
            self.fan_hint = Some(hint);
        }
        // read everything once (and learn what a read costs); the energy counters
        // need a second reading for a first value: soon
        for i in 0..self.srcs.len() {
            self.read_src(i, now);
            if matches!(self.srcs[i].kind, S_RAPL | S_PERF) {
                self.srcs[i].last = now - 10 * SEC;
            }
            // (the BMC answers in milliseconds: its first readings now, not a read later)
            if self.srcs[i].kind == S_BMC {
                self.read_bmc(i, 300);
            }
        }
    }

    /// One hwmon chip's fans, temperatures, voltages and power readings.
    fn hwmon(&mut self, d: &str, name: &str, asus: bool) {
        let dev = std::fs::canonicalize(pb(&[d, "/device"]).s()).map_or(String::new(), |p| p.to_string_lossy().into_owned());
        let driver = std::fs::read_link(pb(&[d, "/device/driver"]).s())
            .ok()
            .and_then(|l| l.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let (mut g, what, class) = chip_info(name);
        let mut what = if what.is_empty() { name } else { what };
        let mut devs = dev.rsplit('/').next().unwrap_or("").to_string();
        if g == G_OTHER {
            if let Some(i) = iface_of(&dev, &self.root) {
                (g, what, devs) = (G_NET, "network chip", i);
            }
        }
        if g == G_DISK {
            devs = [&devs, " ", &rstr(&[&dev, "/model"])].concat();
        }
        let s = self.src(name, &driver, what, &devs, S_HWMON, class, g);
        let files = list(d);
        for f in &files {
            let Some(base) = f.strip_suffix("_input").or(f.strip_suffix("_average")) else {
                continue;
            };
            let Some(i) = base.find(|c: char| c.is_ascii_digit()) else { continue };
            let (pre, n) = (&base[..i], parse_i64(&base.as_bytes()[i..]).unwrap_or(0) as u32);
            let kind = match pre {
                "fan" => K_FAN,
                "temp" => K_TEMP,
                "in" => K_VOLT,
                "power" | "energy" => K_POWER,
                _ => continue,
            };
            // (power1_average and power1_input: one of them, the average first)
            if f.ends_with("_input") && files.iter().any(|x| x.strip_suffix("_average") == Some(base)) {
                continue;
            }
            let lab = rstr(&[d, "/", base, "_label"]);
            let ival = |suf: &str| rint(&[d, "/", base, suf]);
            let mut r = rd(kind, s, g, -1, if lab.is_empty() { base } else { &lab }, "");
            match kind {
                K_FAN => {
                    (r.name, r.raw, r.guess) = fan_name(name, &lab, n, asus);
                    if g == G_OTHER {
                        r.group = G_BOARD;
                    }
                    let pw = pb(&[d, "/pwm", &base[3..]]);
                    r.pwm_fd = openp(&pw);
                    if r.pwm_fd >= 0 {
                        r.en_fd = open_at(&[pw.s(), "_enable"]);
                        r.mode_fd = open_at(&[pw.s(), "_mode"]);
                    }
                }
                K_TEMP => {
                    let (pn, tg, fl) = temp_name(name, &lab, n);
                    if fl & F_DROP != 0 {
                        continue;
                    }
                    (r.name, r.flags, r.minor, r.guess) = (pn, fl, fl & F_MINOR != 0, fl & F_AUX != 0);
                    if g != G_NET {
                        r.group = tg;
                    }
                    (r.warn, r.crit) = temp_limits(r.group, ival("_max"), ival("_crit"));
                }
                K_VOLT => {
                    let mul;
                    (r.name, mul) = volt_name(name, &lab, n, ival("_input").unwrap_or(0) as f32 / 1000.0);
                    r.mul = mul;
                    let lim = |k: &str| ival(k).map_or(0.0, |v| v as f32 / 1000.0 * mul);
                    (r.warn, r.crit) = (lim("_min"), lim("_max"));
                    r.guess = lab.is_empty() && r.name.starts_with("in");
                }
                _ => {
                    let gpu = g == G_GPU;
                    // (an APU's amdgpu has a northbridge voltage)
                    r.name = if !gpu {
                        [name, " ", &r.raw].concat()
                    } else if name == "amdgpu" && exists(&[d, "/in1_input"]) {
                        "GPU (built-in)".into()
                    } else {
                        "GPU".into()
                    };
                    r.role = if gpu { R_GPU } else { R_OTHER };
                    if pre == "energy" {
                        r.ctr = Some(Ctr {
                            scale: 1e-6,
                            ..Default::default()
                        });
                    }
                }
            }
            r.fd = open_at(&[d, "/", f]);
            self.add(s, r);
        }
    }

    /// RAPL energy counters: powercap if readable, else perf (package; per core on
    /// AMD), else note that they exist but are locked.
    fn rapl(&mut self) {
        let root = self.root.clone();
        let pc = [&root, "/sys/class/powercap/"].concat();
        let mut src = usize::MAX;
        let mut found = false;
        for z in list(&pc) {
            // intel-rapl:0, intel-rapl:0:0 (AMD zones are called intel-rapl too)
            let name = rstr(&[&pc, &z, "/name"]);
            let sub = z.matches(':').count() > 1;
            let (role, g, pn) = match (sub, name.as_str()) {
                (false, n) if n.starts_with("package") => (R_PKG, G_CPU, "CPU package"),
                (false, "psys") => (R_PSYS, G_OTHER, "whole computer"),
                (true, "core") if !self.amd => (R_CORES, G_CPU, "cores"),
                (true, "uncore") => (R_UNCORE, G_GPU, "GPU (built-in)"),
                (true, "dram") => (R_DRAM, G_MEM, "memory (RAM)"),
                _ => continue,
            };
            found = true;
            let fd = open_at(&[&pc, &z, "/energy_uj"]);
            self.rapl_locked |= fd < 0;
            if src == usize::MAX && fd >= 0 {
                src = self.src("RAPL", "powercap", "energy counters", "", S_RAPL, C_POWER, G_CPU);
            }
            let mut r = rd(K_POWER, src, g, fd, &[&z, " ", &name].concat(), pn);
            r.role = role;
            r.ctr = Some(Ctr {
                range: rint(&[&pc, &z, "/max_energy_range_uj"]).unwrap_or(0) as u64,
                scale: 1e-6,
                ..Default::default()
            });
            if fd >= 0 {
                self.add(src, r);
            }
        }
        // perf: the package (if powercap is locked) and each core (AMD)
        self.perf_paranoid = rint(&[&root, "/proc/sys/kernel/perf_event_paranoid"]).unwrap_or(-9) as i32;
        for role in [R_PKG, R_CORE] {
            let (pmu, evn) = if role == R_PKG { ("power", "energy-pkg") } else { ("power_core", "energy-core") };
            let d = [&root, "/sys/bus/event_source/devices/", pmu].concat();
            let Some(cfg) = perf_event_cfg(&rstr(&[&d, "/events/", evn])) else { continue };
            found = true;
            let cpus = crate::sys::parse_list(&rstr(&[&d, "/cpumask"]));
            if (role == R_PKG && self.role(R_PKG).is_some()) || cpus.is_empty() {
                continue;
            }
            let typ = rint(&[&d, "/type"]).unwrap_or(0) as u32;
            let scale = parse_sci(&rstr(&[&d, "/events/", evn, ".scale"])).unwrap_or(2.3283064365386963e-10);
            let s = self.src(pmu, "perf", "energy counters (perf)", "", S_PERF, if role == R_PKG { C_POWER } else { C_CORES }, G_CPU);
            for (k, &cpu) in cpus.iter().enumerate() {
                // (fixtures: no perf there)
                let fd = if root.is_empty() { perf_open(typ, cfg, cpu as i32) } else { -1 };
                if fd < 0 {
                    self.perf_locked = true;
                    break;
                }
                let (raw, pn) = if role == R_PKG {
                    ([pmu, "/", evn].concat(), "CPU package".to_string())
                } else {
                    (["cpu", &n2s(cpu as u32)].concat(), ["core ", &n2s(k as u32)].concat())
                };
                let mut r = rd(K_POWER, s, G_CPU, fd, &raw, &pn);
                (r.role, r.minor) = (role, role == R_CORE);
                r.ctr = Some(Ctr {
                    perf: true,
                    scale,
                    ..Default::default()
                });
                self.add(s, r);
            }
            if self.srcs[s].n[3] == 0 {
                self.srcs.pop();
            }
        }
        self.cpu_power = if self.role(R_PKG).is_some() {
            P_OK
        } else if found {
            P_LOCKED
        } else {
            P_NONE
        };
    }

    fn batteries(&mut self) {
        let ps = [&self.root, "/sys/class/power_supply/"].concat();
        for b in list(&ps) {
            let d = [&ps, &b, "/"].concat();
            let o = |f: &str| open_at(&[&d, f]);
            let typ = rstr(&[&d, "type"]);
            if rstr(&[&d, "scope"]) == "Device" {
                continue; // a mouse's or keyboard's battery
            }
            if typ == "Mains" && self.ac < 0 {
                self.ac = o("online");
            }
            if typ != "Battery" {
                continue;
            }
            let charge = !exists(&[&d, "energy_now"]);
            let (en, ef) = if charge { ("charge_now", "charge_full") } else { ("energy_now", "energy_full") };
            let fds = [o("capacity"), o("status"), o("power_now"), o("current_now"), o("voltage_now"), o(en), o(ef)];
            let src = match self.srcs.iter().position(|s| s.kind == S_BAT) {
                Some(s) => s,
                None => self.src("battery", "power_supply", "battery & charger", "", S_BAT, C_BAT, G_OTHER),
            };
            let mut r = rd(K_POWER, src, G_OTHER, -1, &b, "battery");
            r.role = R_BAT;
            self.add(src, r);
            self.bats.push(Bat {
                name: b,
                fds,
                charge,
                pct: -1.0,
                w: 0.0,
                st: 0,
                left: 0.0,
                rd: self.rd.len() - 1,
            });
        }
    }

    /// Names that depend on the others: RAM sticks numbered, network chips by
    /// interface, Tctl beside Tdie; the histories of the key readings.
    fn finish_names(&mut self) {
        let mut ram = 0;
        let nram = self.srcs.iter().filter(|s| s.group == G_MEM).count();
        let ndisk = self.srcs.iter().filter(|s| s.group == G_DISK).count();
        let tdie = self.rd.iter().any(|r| r.raw == "Tdie");
        let nccd = self.rd.iter().filter(|r| r.raw.starts_with("Tccd")).count();
        for r in &mut self.rd {
            let s = &self.srcs[r.src as usize];
            if r.kind == K_TEMP {
                if s.group == G_MEM {
                    ram += 1;
                    r.name = if nram > 1 { ["RAM stick ", &n2s(ram)].concat() } else { "RAM".into() };
                    r.raw = [&s.chip, " ", s.dev.rsplit('-').next().unwrap_or("")].concat();
                }
                if s.group == G_DISK && ndisk > 1 {
                    r.name = [&r.name, " (", s.dev.split(' ').next().unwrap_or(""), ")"].concat();
                }
                if s.group == G_NET && !s.chip.starts_with("iwlwifi") {
                    r.name = ["network ", &s.dev].concat();
                }
                if r.raw == "Tctl" && tdie {
                    r.name = "CPU (fan control value)".into();
                    r.minor = true;
                }
                r.minor |= r.raw.starts_with("Tccd") && nccd > 4;
            }
        }
        self.hists(true);
    }

    /// The key readings' histories (4 KB each): made for the page, dropped while the
    /// overview (which shows none) has the sensors.
    fn hists(&mut self, on: bool) {
        for r in &mut self.rd {
            if !on {
                r.hist = None;
            } else if r.hist.is_none() && ((r.kind == K_TEMP && !r.minor) || matches!(r.role, R_PKG | R_GPU | R_BAT | R_CORES | R_PSYS)) {
                r.hist = Some(Box::new(Ring::new()));
            }
        }
    }

    /// Entering the page: find the sensors (the first time) or read them, and mark
    /// the time away as a gap in the histories.
    pub fn enter(&mut self, now: i64) {
        if !self.detected {
            self.detect(now);
        } else {
            self.hists(true);
            for i in 0..self.srcs.len() {
                self.read_src(i, now);
            }
        }
        if self.last_seen != i64::MIN {
            let gap = ((now - self.last_seen) / SEC).clamp(0, 1024) as u32;
            if gap > 1 {
                self.push_hist(gap - 1, true);
            }
        }
        self.last_seen = now;
    }

    /// How often source `i` is read (ns): its class's base period, or less often if
    /// a read costs more than the class's share of the budget.
    pub fn period(&self, i: usize, eco: bool) -> i64 {
        let s = &self.srcs[i];
        let c = CADENCE[s.class];
        let (base, share) = if eco { (c.0, c.2) } else { (c.1, c.3) };
        (base * 1_000_000).max(s.cost * 1_000_000 / share)
    }

    /// The sensors' planned CPU time (ns per second) at these periods: the eco loop
    /// keeps this much of its budget for them.
    pub fn planned(&self, eco: bool) -> i64 {
        (0..self.srcs.len())
            .map(|i| self.srcs[i].cost * SEC / self.period(i, eco).max(1))
            .sum()
    }

    /// CPU time of the reads since the last call.
    pub fn take_spent(&mut self) -> i64 {
        std::mem::take(&mut self.spent)
    }
    pub fn spent(&self) -> i64 {
        self.spent
    }

    /// A sample while the page is shown: read the sources that are due, then add
    /// `push` history points.
    pub fn tick(&mut self, now: i64, push: u32, eco: bool) {
        if !self.detected {
            self.enter(now);
        }
        // (eco: one dear read (over 0.2 ms) at a time, 30 s apart unless twice
        // overdue, so their costs don't pile up in the same minute)
        let mut ld = i64::MIN;
        for s in &self.srcs {
            if s.cost > 200_000 {
                ld = ld.max(s.last);
            }
        }
        for i in 0..self.srcs.len() {
            let s = &self.srcs[i];
            let (p, late, dear) = (self.period(i, eco), now - s.last, s.cost > 200_000);
            if s.last == i64::MIN || (late >= p - p / 10 && !(eco && dear && now - ld < 30 * SEC && late < 2 * p)) {
                self.read_src(i, now);
                if dear {
                    ld = now;
                }
            }
        }
        if push > 0 {
            self.push_hist(push, false);
        }
        self.last_seen = now;
    }

    /// The CPU page (view 1): only the CPU package's power (RAPL or perf, with the
    /// RAM's where the same source has it), on its own cadence.
    pub fn tick_power(&mut self, now: i64, eco: bool) {
        if !self.detected {
            // (finding the sensors, once, is sensor time too: eco mustn't charge the
            // page's own budget for it)
            let (t0, s0) = (thread_ns(), self.spent);
            self.detect(now);
            self.spent = s0 + (thread_ns() - t0).max(0);
        }
        let Some(s) = self.role(R_PKG).map(|i| self.rd[i].src as usize) else { return };
        let (p, l) = (self.period(s, eco), self.srcs[s].last);
        if l == i64::MIN || now - l >= p - p / 10 {
            self.read_src(s, now);
        }
    }

    /// The CPU package's watts and the RAM's (where read), for the CPU page.
    pub fn cpu_watts(&self) -> (Option<f32>, Option<f32>) {
        let w = |role| self.role(role).filter(|&i| self.cpu_power == P_OK && self.rd[i].ok).map(|i| self.rd[i].val);
        (w(R_PKG), w(R_DRAM))
    }

    /// The overview (view 0) reads source `i`: not the per-core power counters, not
    /// the CPU's temperatures when the CPU sampler has them (`cpu_t`), not the slow
    /// ones (RAM sticks and drives on the SMBus / admin queue block for up to ~10 ms:
    /// the memory page's helper thread reads those, firmware zones, Wi-Fi).
    pub fn ov_wants(&self, i: usize, cpu_t: bool) -> bool {
        let c = self.srcs[i].class;
        c != C_CORES && c != C_SLOW && !(cpu_t && c == C_CPU && self.srcs[i].n[K_POWER as usize] == 0)
    }

    /// How often the overview reads source `i`: as the page does, but in eco the
    /// temperatures every 30 s, dearer reads every 2 minutes, and a dear one (a
    /// board's sensor chip: 2.5 ms) every OV_DEAR; CPU power as the page does.
    pub fn ov_period(&self, i: usize, eco: bool) -> i64 {
        let (p, c) = (self.period(i, eco), self.srcs[i].cost);
        if !eco {
            // (smooth & co: a dear one every 30 s, the fan isn't the point here)
            if self.ov_dear_src(i) { p.max(30 * SEC) } else { p }
        } else if self.ov_dear_src(i) {
            p.max(OV_DEAR)
        } else if c > 40_000 {
            // (a GPU's power reading: a message to its firmware, 50-400 µs; RAM sticks'
            // sensors on the SMBus, ~95 µs)
            p.max(120 * SEC)
        } else if self.srcs[i].class == C_POWER {
            p
        } else {
            p.max(30 * SEC)
        }
    }

    /// A dear source: a read costs over 0.2 ms, or it is a board chip on the LPC bus
    /// (Nuvoton, ITE, Winbond, Fintek: 2.5 ms when its cache is stale, cheap when
    /// another program just read it) or behind SMM (Dell).
    fn ov_dear_src(&self, i: usize) -> bool {
        let s = &self.srcs[i];
        s.cost > 200_000 || ["nct", "it8", "w83", "f71", "dell"].iter().any(|p| s.chip.starts_with(p))
    }

    /// The overview's planned sensor CPU time (ns a second), and in eco while a dear
    /// read is within a minute (before or after), that read's cost spread over the
    /// minute: the rest then runs slower, so no 60 s window holds more than the
    /// budget.
    pub fn ov_planned(&self, now: i64, eco: bool, cpu_t: bool) -> i64 {
        let mut p = 0;
        for i in 0..self.srcs.len() {
            if self.ov_wants(i, cpu_t) {
                p += self.srcs[i].cost * SEC / self.ov_period(i, eco).max(1);
            }
        }
        p + self.ov_near(now, eco, cpu_t) / 60
    }

    /// Eco on the overview: the cost (ns) of a dear read that happened in the last
    /// minute or is due in the next one (a read when the page came into view is the
    /// user's, not counted), else 0.
    pub fn ov_near(&self, now: i64, eco: bool, cpu_t: bool) -> i64 {
        let mut c = 0;
        for i in 0..self.srcs.len() {
            let s = &self.srcs[i];
            if eco && self.ov_dear_src(i) && self.ov_wants(i, cpu_t) && s.last != i64::MIN {
                let next = s.last + self.ov_period(i, eco);
                if now - self.ov_dear < 60 * SEC || next - now < 60 * SEC {
                    c = c.max(s.cost.max(2_500_000));
                }
            }
        }
        c
    }

    /// A sample on the overview: the sources it shows that are due (all of them
    /// when `enter`), then `push` history points.
    pub fn tick_ov(&mut self, now: i64, push: u32, eco: bool, cpu_t: bool, enter: bool) {
        if !self.detected {
            self.enter(now);
        }
        if enter {
            self.hists(false);
        }
        for i in 0..self.srcs.len() {
            let (p, l) = (self.ov_period(i, eco), self.srcs[i].last);
            if self.ov_wants(i, cpu_t) && (enter || l == i64::MIN || now - l >= p - p / 10) {
                self.read_src(i, now);
                if !enter && self.ov_dear_src(i) {
                    self.ov_dear = now;
                }
            }
        }
        // (no histories here: the page marks the time away as a gap)
        let _ = push;
    }

    fn push_hist(&mut self, n: u32, gap: bool) {
        for r in &mut self.rd {
            if r.kind == K_FAN && r.seen && r.hist.is_none() {
                r.hist = Some(Box::new(Ring::new()));
            }
            let Some(h) = r.hist.as_mut() else { continue };
            let v = if r.ok && !gap { r.val } else { 0.0 };
            for _ in 0..n.min(1024) {
                h.push(v);
            }
            // the highest value of the last 2 minutes (a full look every 10 points)
            r.peak = if self.pts % 10 == 0 || n > 1 {
                (0..h.len().min(120)).map(|k| h.back(k)).fold(0.0, f32::max)
            } else {
                r.peak.max(v)
            };
        }
        self.pts += n as u64;
    }

    /// Read one source and account its CPU time.
    pub fn read_src(&mut self, i: usize, now: i64) {
        let t0 = thread_ns();
        let reads = self.srcs[i].reads;
        let kind = self.srcs[i].kind;
        let mut board: Option<f32> = None;
        if kind == S_BAT {
            self.read_bats(now);
        }
        if kind == S_BMC {
            self.read_bmc(i, 0);
        }
        for r in &mut self.rd {
            if r.src as usize != i || r.fd < 0 || kind == S_BMC {
                continue;
            }
            match r.kind {
                K_FAN => {
                    let v = read_fd(r.fd);
                    r.ok = v.is_some_and(|v| (0..30000).contains(&v));
                    r.val = if r.ok { v.unwrap_or(0) as f32 } else { 0.0 };
                    if r.val > 0.0 {
                        r.seen = true;
                        r.max = r.max.max(r.val);
                    }
                    if r.seen && r.pwm_fd >= 0 {
                        if let Some(p) = read_fd(r.pwm_fd) {
                            r.pwm = p as f32 / 255.0;
                        }
                        if reads % 8 == 0 {
                            r.en = read_fd(r.en_fd).map_or(-1, |v| v.clamp(0, 9) as i8);
                            r.dc = read_fd(r.mode_fd).map_or(-1, |v| (v == 0) as i8);
                        }
                    }
                }
                K_TEMP => {
                    let v = read_fd(r.fd).map(|v| v as f32 / 1000.0);
                    r.val = v.unwrap_or(0.0);
                    r.ok = v.is_some_and(|v| temp_ok(v, false, None));
                    if r.flags & F_REF != 0 && r.ok {
                        board = Some(r.val);
                    }
                }
                K_VOLT => {
                    let v = read_fd(r.fd);
                    r.val = v.unwrap_or(0) as f32 / 1000.0 * r.mul;
                    r.ok = v.is_some_and(|v| v != 0);
                }
                K_POWER => {
                    if let Some(c) = r.ctr.as_mut() {
                        let v = if c.perf {
                            let mut b = 0u64;
                            let n = unsafe { libc::read(r.fd, &mut b as *mut u64 as *mut _, 8) };
                            (n == 8).then_some(b)
                        } else {
                            read_fd(r.fd).map(|v| v as u64)
                        };
                        if let Some(w) = v.and_then(|v| c.update(v, now)) {
                            r.val = w;
                            r.ok = true;
                        }
                    } else {
                        let v = read_fd(r.fd);
                        r.ok = v.is_some_and(|v| v >= 0);
                        r.val = v.unwrap_or(0) as f32 / 1e6;
                    }
                }
                _ => {}
            }
            if r.ok {
                r.seen = r.seen || r.kind != K_FAN;
            }
        }
        // unidentified board inputs: real only if not colder than the board
        for r in &mut self.rd {
            if r.src as usize == i && r.kind == K_TEMP && r.flags & F_AUX != 0 && r.ok {
                r.ok = temp_ok(r.val, true, board);
            }
        }
        let c = (thread_ns() - t0).max(0);
        let s = &mut self.srcs[i];
        s.cost = if c > s.cost { c } else { s.cost - (s.cost - c) / 8 };
        s.last = now;
        s.reads += 1;
        self.spent += c;
    }

    /// The BMC's answers to the last read's requests (waiting up to `wait_ms` for
    /// them), then the next requests.
    fn read_bmc(&mut self, i: usize, wait_ms: i32) {
        use crate::ipmi::reading;
        let Some(b) = self.bmc.as_ref() else { return };
        let mut buf = [0u8; 288];
        let mut left = self.rd.iter().filter(|r| r.src as usize == i && r.flags & F_ASKED != 0).count();
        let t0 = crate::term::mono();
        loop {
            while let Some((id, d)) = b.recv(&mut buf) {
                let Some(r) = self.rd.get_mut(id as usize).filter(|r| r.src as usize == i && r.flags & F_ASKED != 0) else { continue };
                r.flags &= !F_ASKED;
                left = left.saturating_sub(1);
                let v = reading(d).map(|x| self.sdr[r.fd as usize].value(x));
                r.val = v.unwrap_or(0.0);
                r.ok = match r.kind {
                    K_FAN => v.is_some_and(|v| (0.0..60000.0).contains(&v)),
                    K_TEMP => v.is_some_and(|v| temp_ok(v, false, None)),
                    _ => v.is_some_and(|v| v.is_finite() && v > 0.0),
                };
                if r.ok {
                    r.seen |= r.kind != K_FAN || r.val > 0.0;
                    r.max = r.max.max(r.val);
                }
            }
            let gone = (crate::term::mono() - t0) / 1_000_000;
            if left == 0 || gone >= wait_ms as i64 {
                break;
            }
            let mut p = libc::pollfd {
                fd: b.fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut p, 1, wait_ms - gone as i32) };
        }
        for k in 0..self.rd.len() {
            let r = &mut self.rd[k];
            if r.src as usize == i && b.ask(&self.sdr[r.fd as usize], k) {
                r.flags |= F_ASKED;
            }
        }
    }

    /// The BMC's sensors: fans, temperatures (by what their names say they
    /// measure) and voltages, with the limits the BMC sets.
    fn add_bmc(&mut self, sdr: Vec<crate::ipmi::Sdr>) {
        use crate::ipmi::{T_FAN, T_TEMP};
        if sdr.is_empty() {
            return;
        }
        // (read as often as a board chip: cadence class 1)
        let s = self.src("BMC", "ipmi_si", "server board's BMC (IPMI)", "", S_BMC, 1, G_BOARD);
        let cpu_t = self.rd.iter().any(|r| r.kind == K_TEMP && r.group == G_CPU);
        for (k, x) in sdr.iter().enumerate() {
            let lc = x.name.to_ascii_lowercase();
            let (kind, name) = match x.typ {
                // ("FAN1" → "fan 1", "FANA" → "fan A")
                T_FAN => {
                    let n = x.name.get(..3).filter(|p| p.eq_ignore_ascii_case("fan")).map_or(x.name.as_str(), |_| &x.name[3..]);
                    (K_FAN, ["fan ", n.trim_start_matches([' ', '_'])].concat())
                }
                T_TEMP => (K_TEMP, x.name.trim_end_matches(" Temp").trim_end_matches("_Temp").to_string()),
                _ => (K_VOLT, x.name.clone()),
            };
            let g = if lc.contains("cpu") {
                G_CPU
            } else if lc.contains("dimm") || lc.contains("mem") {
                G_MEM
            } else if lc.contains("nvme") || lc.contains("ssd") || lc.contains("hdd") {
                G_DISK
            } else if lc.contains("nic") || lc.contains("lan") {
                G_NET
            } else {
                G_BOARD
            };
            let mut r = rd(kind, s, if kind == K_FAN { G_BOARD } else { g }, k as c_int, &x.name, if name.trim().is_empty() { &x.name } else { &name });
            match kind {
                K_TEMP => {
                    let ml = |v: Option<f32>| v.map(|v| (v * 1000.0) as i64);
                    (r.warn, r.crit) = temp_limits(r.group, ml(x.unc), ml(x.uc));
                    // (the CPU's own sensor is the one to watch)
                    r.minor = g == G_CPU && cpu_t;
                }
                K_VOLT => (r.warn, r.crit) = (x.lc.or(x.lnc).unwrap_or(0.0), x.uc.or(x.unc).unwrap_or(0.0)),
                _ => {}
            }
            self.add(s, r);
        }
        self.sdr = sdr;
    }

    fn read_bats(&mut self, _now: i64) {
        if self.ac >= 0 {
            self.ac_on = read_fd(self.ac).map_or(-1, |v| (v != 0) as i8);
        }
        for b in &mut self.bats {
            let g = |k: usize| read_fd(b.fds[k]);
            b.pct = g(B_CAP).map_or(-1.0, |v| v as f32);
            let mut buf = [0u8; 24];
            let n = pread(b.fds[B_STATUS], &mut buf);
            b.st = match &buf[..n.min(4)] {
                b"Char" => 1,
                b"Disc" => 2,
                b"Full" => 3,
                b"Not " => 4,
                _ => 0,
            };
            // µW, or µA × µV
            let v = g(B_VNOW).unwrap_or(0) as f64;
            let w = match g(B_PNOW) {
                Some(p) => p.unsigned_abs() as f64 / 1e6,
                None => g(B_CNOW).map_or(0.0, |c| c.unsigned_abs() as f64 * v / 1e12),
            };
            b.w = w as f32;
            // energy left / to fill (µWh, or µAh × V)
            let k = if b.charge { v / 1e6 } else { 1.0 };
            let (e, f) = (
                g(B_ENOW).map(|x| x as f64 * k / 1e6),
                g(B_EFULL).map(|x| x as f64 * k / 1e6),
            );
            b.left = match (b.st, e, f) {
                (2, Some(e), _) if w > 0.05 => (e / w * 3600.0) as f32,
                (1, Some(e), Some(f)) if w > 0.05 && f > e => ((f - e) / w * 3600.0) as f32,
                _ => 0.0,
            };
        }
        for k in 0..self.bats.len() {
            let (i, w) = (self.bats[k].rd, self.bats[k].w);
            self.rd[i].val = w;
            self.rd[i].ok = self.bats[k].pct >= 0.0;
        }
    }

    // ── queries for the page ───────────────────────────────────────────────

    /// The CPU counts energy per core (perf power_core), whether or not readable.
    pub fn has_core_pmu(&self) -> bool {
        exists(&[&self.root, "/sys/bus/event_source/devices/power_core/type"])
    }

    /// Distinct sensor drivers (two DIMM sensors are one).
    pub fn ndrivers(&self) -> usize {
        let mut v: Vec<&str> = Vec::new();
        for s in &self.srcs {
            if s.kind == S_HWMON || s.kind == S_ZONES {
                let c = s.chip.split("_0").next().unwrap_or("");
                if !v.contains(&c) {
                    v.push(c);
                }
            }
        }
        v.len()
    }

    /// The first reading of a role (CPU package...).
    pub fn role(&self, role: u8) -> Option<usize> {
        self.rd.iter().position(|r| r.kind == K_POWER && r.role == role)
    }

    /// Per-core watts (AMD, perf): their sum, once every core has a value.
    pub fn cores_sum(&self) -> Option<f32> {
        if let Some(i) = self.role(R_CORES) {
            return self.rd[i].ok.then_some(self.rd[i].val);
        }
        let mut n = 0;
        let mut s = 0.0;
        for r in &self.rd {
            if r.role == R_CORE {
                if !r.ok {
                    return None;
                }
                n += 1;
                s += r.val;
            }
        }
        (n > 0).then_some(s)
    }

    /// Seconds since source `s` was read.
    pub fn age(&self, s: usize, now: i64) -> i64 {
        let l = self.srcs[s].last;
        if l == i64::MIN {
            0
        } else {
            (now - l) / SEC
        }
    }
}

// ── cpuview --setup-sensors ─────────────────────────────────────────────────

const UDEV_RULE: &str = "/etc/udev/rules.d/70-cpuview-rapl.rules";
const BMC_RULE: &str = "/etc/udev/rules.d/70-cpuview-ipmi.rules";

/// The `--bmc` step for user `u`: what it does and why, then the commands.
fn bmc_step(u: &str) -> String {
    [
        "let ", u, " read the board's BMC (fans, temperatures, voltages through\n",
        "# IPMI), now and after every boot. /dev/ipmi0 takes any command, not only\n",
        "# readings: whoever opens it can also power the machine off or change the\n",
        "# BMC's users and network. This gives it to ", u, " alone (who can do all\n",
        "# that with sudo already), not to other users.\n",
        "printf '%s\\n' '# cpuview --setup-sensors --bmc: let ", u, " read the BMC (IPMI)' \\\n",
        "  '# remove with: cpuview --setup-sensors --undo' \\\n",
        "  'SUBSYSTEM==\"ipmi\", KERNEL==\"ipmi[0-9]*\", OWNER=\"", u, "\", MODE=\"0600\"' > ", BMC_RULE,
        "\nudevadm control --reload\nfor d in /dev/ipmi[0-9]*; do [ -e \"$d\" ] && chown ", u, " \"$d\"; done; true",
    ]
    .concat()
}

/// This user's login name, if it is a plain one (it goes into a shell command and a
/// udev rule).
fn user_name() -> Option<String> {
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if pw.is_null() {
        return None;
    }
    let n = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_string_lossy().into_owned();
    (!n.is_empty() && n.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) && !n.starts_with('-')).then_some(n)
}

/// The RAPL zones' energy_uj files whose readability (by others) is `readable`.
fn rapl_files(readable: bool) -> String {
    let mut v = String::new();
    for z in list("/sys/class/powercap") {
        let f = ["/sys/class/powercap/", &z, "/energy_uj"].concat();
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if z.contains("rapl:") && unsafe { libc::stat(pb(&[&f]).b.as_ptr() as *const _, &mut st) } == 0 && (st.st_mode & 0o004 != 0) == readable {
            v += " ";
            v += &f;
        }
    }
    v
}

fn has_cap(exe: &str) -> bool {
    unsafe { libc::getxattr(pb(&[exe]).b.as_ptr() as *const _, c"security.capability".as_ptr(), std::ptr::null_mut(), 0) > 0 }
}

/// `cpuview --setup-sensors [--undo] [--per-core] [--bmc] [--dry-run]`: show the
/// shell commands it would run as root (each step with what it changes and why),
/// ask, then run exactly those with sudo. Returns the exit code.
pub fn setup(undo: bool, per_core: bool, bmc: bool, dry: bool) -> i32 {
    let exe = std::fs::read_link("/proc/self/exe").map_or("cpuview".into(), |p| p.to_string_lossy().into_owned());
    let mut sh = String::new();
    let mut note = String::new();
    let step = |sh: &mut String, what: &[&str]| {
        sh.push_str("\n# ");
        for w in what {
            sh.push_str(w);
        }
        sh.push('\n');
    };
    let pmu = exists(&["/sys/bus/event_source/devices/power_core"]);
    let cap = has_cap(&exe);
    if undo {
        for f in list("/etc/modules-load.d") {
            if let Some(m) = f.strip_prefix("cpuview-").and_then(|f| f.strip_suffix(".conf")) {
                step(&mut sh, &["stop loading the ", m, " driver at boot, and unload it\nrm -f /etc/modules-load.d/", &f, "\nmodprobe -r ", m]);
            }
        }
        let files = rapl_files(true);
        if exists(&[UDEV_RULE]) || !files.is_empty() {
            step(&mut sh, &["make the RAPL energy counters root-only again\nrm -f ", UDEV_RULE, "\nudevadm control --reload"]);
            if !files.is_empty() {
                sh += &["chmod go-r", &files, "\n"].concat();
            }
        }
        if cap {
            step(&mut sh, &["remove the perfmon capability\nsetcap -r ", &exe]);
        }
        if exists(&[BMC_RULE]) {
            step(&mut sh, &["make the BMC's device root-only again\nrm -f ", BMC_RULE, "\nudevadm control --reload\nfor d in /dev/ipmi[0-9]*; do [ -e \"$d\" ] && chown root \"$d\"; done; true"]);
        }
    } else {
        let mut s = Sens::empty();
        s.detect(crate::term::mono());
        match &s.fan_hint {
            // a guess by the board's maker: kept for every boot only if it finds the chip,
            // and a miss doesn't stop the other steps
            Some((m, 0)) => step(&mut sh, &[
                "try the ", m, " driver: no fan sensors are visible, and boards from this maker\n",
                "# usually report their fans through it. Loaded at every boot only if it finds the chip.\n",
                "if modprobe ", m, "; then\n  echo ", m, " > /etc/modules-load.d/cpuview-", m, ".conf\n",
                "else\n  echo 'fans: the ", m, " driver found no fan chip on this board; `sudo sensors-detect` (lm-sensors) can look for the right one'\n",
                "  rm -f /etc/modules-load.d/cpuview-", m, ".conf\nfi",
            ]),
            Some((_, 3)) if !s.bmc_locked => note += "fans: this board's BMC runs the fans (IPMI) but lists none; `sudo ipmitool sdr type Fan` shows what it has\n",
            Some((_, 3)) => {}
            Some((m, 1)) => note += &["fans: the ", m, " driver is loaded but shows no fans; `sudo sensors-detect` (lm-sensors) may find the right one\n"].concat(),
            Some(_) => note += "fans: no fan sensors and no known driver for this board; `sudo sensors-detect` (lm-sensors) can look for one\n",
            None if s.vm => note += "this is a virtual machine: fans, temperatures and power belong to the host\n",
            None => {}
        }
        let files = rapl_files(false);
        if !files.is_empty() {
            step(&mut sh, &[
                "let every user read the CPU's RAPL energy counters (CPU power = energy over\n",
                "# time), now and after every boot. The kernel makes them root-only because\n",
                "# RAPL readings can leak information about what other users' programs do\n",
                "# (power side channels, e.g. the 2020 PLATYPUS attack). On a single-user\n",
                "# machine that is no concern.\n",
                "printf '%s\\n' '# cpuview --setup-sensors: let every user read the RAPL energy counters (CPU power)' \\\n",
                "  '# remove with: cpuview --setup-sensors --undo' \\\n",
                "  'SUBSYSTEM==\"powercap\", ACTION==\"add\", RUN+=\"/bin/chmod a+r /sys%p/energy_uj\"' > ", UDEV_RULE,
                "\nudevadm control --reload\nchmod a+r", &files,
            ]);
        } else if !exists(&["/sys/class/powercap/intel-rapl:0"]) {
            note += "CPU power: this CPU has no RAPL energy counters in /sys/class/powercap\n";
        } else {
            note += "CPU power: the RAPL counters are already readable\n";
        }
        let root = unsafe { libc::geteuid() } == 0;
        match (s.bmc_locked, bmc, user_name()) {
            (true, true, Some(u)) if !root => step(&mut sh, &[&bmc_step(&u)]),
            (true, false, _) if !root => note += "fans: this board's BMC runs the fans (IPMI), root-only: cpuview --setup-sensors --bmc lets you read them\n",
            _ => {}
        }
        if per_core && pmu && !cap {
            step(&mut sh, &[
                "give cpuview the perfmon capability, for per-core power: per-core energy\n",
                "# counters are perf events, which this kernel allows only with CAP_PERFMON.\n",
                "# File capabilities are lost when the binary is replaced: run\n",
                "# cpuview --setup-sensors --per-core again after every reinstall.\nsetcap cap_perfmon=ep ", &exe,
            ]);
        } else if pmu && !cap {
            note += "per-core power is possible too: cpuview --setup-sensors --per-core (redo after each reinstall)\n";
        }
    }
    let root = unsafe { libc::geteuid() } == 0;
    let say = |s: &str| crate::term::write_all(1, s.as_bytes());
    say(&note);
    if sh.is_empty() {
        say("cpuview --setup-sensors: nothing to do.\n");
        return 0;
    }
    say(&["cpuview --setup-sensors will run this as root", if root { "" } else { " (with sudo)" }, ":\n", sh.replace('\n', "\n  ").trim_end(), "\n"].concat());
    if dry {
        say("\n(dry run: nothing was changed)\n");
        return 0;
    }
    say("\nGo ahead? [y/N] ");
    let mut b = [0u8; 16];
    let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut _, b.len()) };
    if n < 1 || (b[0] | 0x20) != b'y' {
        say("nothing changed\n");
        return 1;
    }
    let sh = ["set -e\n", &sh].concat();
    let args = ["sudo", "sh", "-c", &sh];
    if crate::config::run(&args[root as usize..], false) == Ok(0) {
        say(if undo { "done\n" } else { "done — restart cpuview and press 5\n" });
        0
    } else {
        say("failed: nothing more was changed after the step that failed\n");
        1
    }
}

#[cfg(test)]
#[path = "sensors_test.rs"]
mod tests;

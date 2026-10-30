//! CPU temperatures through PawnIO, for the sensor service (win/svc.rs): Intel's
//! digital thermal sensors (each core's IA32_THERM_STATUS and the package's, in
//! degrees below TjMax from IA32_TEMPERATURE_TARGET; read on the core itself) and
//! AMD's (Tctl and each CCD's over SMN, as Linux's k10temp reads them). Read only:
//! the modules allow these reads and nothing here writes.

use super::pawnio::Module;
use super::svc::Reading;
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadGroupAffinity};

const MSR_IA32_THERM_STATUS: u64 = 0x19c;
const MSR_IA32_TEMPERATURE_TARGET: u64 = 0x1a2;
const MSR_IA32_PACKAGE_THERM_STATUS: u64 = 0x1b1;
/// SMN: the reported temperature (Tctl), and the CCDs' from here + an offset by model
const ZEN_REPORTED_TEMP: u64 = 0x0005_9800;

pub enum Cpu {
    /// the module, TjMax, and per core its label and the processor group and number
    /// of its first thread
    Intel { m: Module, tjmax: u32, cores: Vec<(String, u16, u8)> },
    /// the module, the CCDs' SMN offset and the CCDs that report
    Amd { m: Module, ccd_offset: u64, ccds: Vec<u64> },
}

/// Vendor ("GenuineIntel", "AuthenticAMD"), family and model (CPUID leaf 0 and 1;
/// other CPUs: none this reads).
#[cfg(not(target_arch = "x86_64"))]
fn cpuid() -> (String, u32, u32) {
    (String::new(), 0, 0)
}

#[cfg(target_arch = "x86_64")]
fn cpuid() -> (String, u32, u32) {
    use core::arch::x86_64::__cpuid;
    let l0 = __cpuid(0);
    let mut v = Vec::with_capacity(12);
    for r in [l0.ebx, l0.edx, l0.ecx] {
        v.extend_from_slice(&r.to_le_bytes());
    }
    let a = __cpuid(1).eax;
    let (fam, model) = ((a >> 8) & 0xf, (a >> 4) & 0xf);
    let family = if fam == 0xf { fam + ((a >> 20) & 0xff) } else { fam };
    let model = if fam == 0xf || fam == 6 { model | ((a >> 16) & 0xf) << 4 } else { model };
    (String::from_utf8_lossy(&v).into_owned(), family, model)
}

impl Cpu {
    /// The CPU's reader, with its module from `dir` (IntelMSR.bin, AMDFamily17.bin),
    /// if the CPU is one it knows and the driver loads it.
    pub fn open(dir: &std::path::Path) -> Option<Cpu> {
        let (vendor, family, model) = cpuid();
        if vendor == "GenuineIntel" {
            let m = Module::load(&std::fs::read(dir.join("IntelMSR.bin")).ok()?)?;
            let target = m.read("ioctl_read_msr", MSR_IA32_TEMPERATURE_TARGET).unwrap_or(0);
            let tjmax = match ((target >> 16) & 0xff) as u32 {
                0 => 100,
                t => t,
            };
            let t = crate::sys::Topo::detect();
            let gn = crate::sys::win::group_numbers();
            let cores = t
                .groups
                .iter()
                .enumerate()
                .filter_map(|(k, g)| gn.get(*t.cpus.get(*g.first()?)?).map(|&(gr, n)| (["Core ", &k.to_string()].concat(), gr, n)))
                .collect();
            Some(Cpu::Intel { m, tjmax, cores })
        } else if vendor == "AuthenticAMD" {
            let (ccd_offset, max) = ccd_offset(family, model)?;
            let m = Module::load(&std::fs::read(dir.join("AMDFamily17.bin")).ok()?)?;
            let ccds = (0..max)
                .filter(|&i| m.read("ioctl_read_smn", ZEN_REPORTED_TEMP + ccd_offset + 4 * i).is_some_and(|v| amd_ccd(v as u32).is_some()))
                .collect();
            Some(Cpu::Amd { m, ccd_offset, ccds })
        } else {
            None
        }
    }

    /// Read the temperatures now; Intel's cores' only with `all_cores` (each is read on
    /// its core: the thread visits all of them, waking the idle ones).
    pub fn read(&self, all_cores: bool) -> Vec<Reading> {
        let mut out = Vec::new();
        match self {
            Cpu::Intel { m, tjmax, cores } => {
                if let Some(v) = m.read("ioctl_read_msr", MSR_IA32_PACKAGE_THERM_STATUS).and_then(|s| intel_temp(*tjmax, s, false)) {
                    out.push(Reading::cpu("Pkg", v, false));
                }
                // (each core's sensor is read on the core: the thread moves to it)
                let me = unsafe { GetCurrentThread() };
                let mut prev: GROUP_AFFINITY = unsafe { std::mem::zeroed() };
                let mut moved = false;
                for (label, group, n) in cores.iter().take(if all_cores { usize::MAX } else { 0 }) {
                    let a = GROUP_AFFINITY { Mask: 1usize << n, Group: *group, Reserved: [0; 3] };
                    let ok = unsafe { SetThreadGroupAffinity(me, &a, if moved { std::ptr::null_mut() } else { &mut prev }) } != 0;
                    moved |= ok;
                    if let Some(v) = ok.then(|| m.read("ioctl_read_msr", MSR_IA32_THERM_STATUS)).flatten().and_then(|s| intel_temp(*tjmax, s, true)) {
                        out.push(Reading::cpu(label.clone(), v, true));
                    }
                }
                if moved {
                    unsafe { SetThreadGroupAffinity(me, &prev, std::ptr::null_mut()) };
                }
            }
            Cpu::Amd { m, ccd_offset, ccds } => {
                if let Some(v) = m.read("ioctl_read_smn", ZEN_REPORTED_TEMP) {
                    out.push(Reading::cpu("Tctl", amd_tctl(v as u32), false));
                }
                for (k, &i) in ccds.iter().enumerate() {
                    if let Some(c) = m.read("ioctl_read_smn", ZEN_REPORTED_TEMP + ccd_offset + 4 * i).and_then(|v| amd_ccd(v as u32)) {
                        out.push(Reading::cpu(["Tccd", &(k + 1).to_string()].concat(), c, ccds.len() > 4));
                    }
                }
            }
        }
        out
    }
}

/// °C from an Intel thermal status (digital readout: degrees below TjMax), if the
/// reading is valid (`valid`: the core's register has a valid bit, the package's
/// none).
pub fn intel_temp(tjmax: u32, status: u64, valid: bool) -> Option<f32> {
    if valid && status & (1 << 31) == 0 {
        return None;
    }
    Some(tjmax as f32 - ((status >> 16) & 0x7f) as f32)
}

/// Tctl (°C) from AMD's reported temperature: 0.125 °C steps from bit 21, 49 °C
/// lower in the extended range (range select, or both TJ select bits).
pub fn amd_tctl(v: u32) -> f32 {
    let t = (v >> 21) as f32 * 0.125;
    if v & (1 << 19) != 0 || (v >> 16) & 3 == 3 {
        t - 49.0
    } else {
        t
    }
}

/// A CCD's °C, if its reading is valid (bit 11).
pub fn amd_ccd(v: u32) -> Option<f32> {
    (v & (1 << 11) != 0).then_some((v & 0x7ff) as f32 * 0.125 - 49.0)
}

/// Where the CCDs' temperatures are (offset from the reported temperature) and how
/// many CCDs to look for, by family and model (as k10temp knows them).
pub fn ccd_offset(family: u32, model: u32) -> Option<(u64, u64)> {
    match (family, model) {
        (0x17 | 0x18, 0x01 | 0x08 | 0x11 | 0x18 | 0x31 | 0x47 | 0x60 | 0x68 | 0x71) => Some((0x154, 8)),
        (0x17 | 0x18, 0xa0..=0xaf) => Some((0x300, 8)),
        (0x19, 0x00..=0x01 | 0x08 | 0x21 | 0x50..=0x5f) => Some((0x154, 8)),
        (0x19, 0x40..=0x4f) => Some((0x300, 8)),
        (0x19, 0x60..=0x7f) => Some((0x308, 8)),
        (0x19, 0x10..=0x1f | 0xa0..=0xaf) => Some((0x300, 12)),
        (0x1a, 0x00..=0x1f) => Some((0x1f0, 16)),
        (0x1a, 0x40..=0x4f) => Some((0x308, 8)),
        // (Tctl only)
        (0x17..=0x1a, _) => Some((0, 0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding() {
        // Intel: 100 °C TjMax, 37 below it, valid
        assert_eq!(intel_temp(100, 1 << 31 | 37 << 16, true), Some(63.0));
        assert_eq!(intel_temp(100, 37 << 16, true), None);
        assert_eq!(intel_temp(100, 37 << 16, false), Some(63.0));
        // AMD: 50 °C (400 eighths), and in the extended range
        assert_eq!(amd_tctl(400 << 21), 50.0);
        assert_eq!(amd_tctl(400 << 21 | 1 << 19), 1.0);
        assert_eq!(amd_ccd((99 * 8) | 1 << 11), Some(50.0));
        assert_eq!(amd_ccd(99 * 8), None);
        assert_eq!(ccd_offset(0x19, 0x21), Some((0x154, 8)));
        assert_eq!(ccd_offset(0x19, 0x61), Some((0x308, 8)));
        assert_eq!(ccd_offset(0x1a, 0x44), Some((0x308, 8)));
        assert_eq!(ccd_offset(0x15, 0x02), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn this_cpu() {
        let (v, f, _) = cpuid();
        assert_eq!(v.len(), 12);
        assert!(f >= 6);
    }
}

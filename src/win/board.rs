//! The motherboard's monitoring chip, for the sensor service (win/svc.rs): Nuvoton's
//! NCT6779–NCT6799, the Super I/O on most ASUS boards (and many others) since 2013,
//! and ITE's and Fintek's (win/chips.rs), through PawnIO's LpcIO module: its fans
//! (RPM, duty, how they're controlled), voltages and temperatures, the inputs Linux's
//! nct6775, it87 and f71882fg drivers list (register facts from the chips' datasheets
//! and those drivers; the code is cpuview's own).
//!
//! Guarded, as other monitoring tools are: every access holds the mutex they share
//! for the ISA bus (Global\Access_ISABUS.HTP.Method). Nothing is written but the
//! Super I/O's configuration key, exit and device select, the chip's index and (a
//! Nuvoton's) bank registers and, on NCT6791 and later, the lock the firmware may
//! set on the monitor's I/O window: each read opens it and closes it again, as the
//! board's own firmware does around each access (ASUS's), and puts the device select,
//! index and bank back as they were. Read at most every 1.5 s (as Linux's driver caches it); a
//! chip whose readings make no sense is left alone.

use super::chips::*;
use super::pawnio::Module;
use super::svc::{Reading, B_CHIP, B_FAN, B_TEMP, B_VOLT};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject};

/// the mutex monitoring tools share for the ISA bus (LPC, the Super I/O, SMBus)
const ISA_MUTEX: &str = "Global\\Access_ISABUS.HTP.Method";
/// the chip's hardware monitor: logical device, its base address register; the
/// index / data ports at base + 5 / + 6, the bank select register
const LD_HWM: u64 = 0x0b;
const SIO_ADDR: u64 = 0x60;
const SIO_ENABLE: u64 = 0x30;
const SIO_ID: u64 = 0x20;
const SIO_LDSEL: u64 = 0x07;
/// NCT6791 and later: bit 4 locks the monitor's I/O window
const SIO_HM_LOCK: u64 = 0x28;
const REG_BANK: u8 = 0x4e;
/// how often the chip is read at most (ms)
const EVERY: u64 = 1500;

const REG_FAN: [u16; 7] = [0x4c0, 0x4c2, 0x4c4, 0x4c6, 0x4c8, 0x4ca, 0x4ce];
const REG_PWM: [u16; 7] = [0x01, 0x03, 0x11, 0x13, 0x15, 0xa09, 0xb09];
const REG_FAN_MODE: [u16; 7] = [0x102, 0x202, 0x302, 0x802, 0x902, 0xa02, 0xb02];
const REG_IN: [u16; 18] = [0x480, 0x481, 0x482, 0x483, 0x484, 0x485, 0x486, 0x487, 0x488, 0x489, 0x48a, 0x48b, 0x48c, 0x48d, 0x48e, 0x48f, 0x470, 0x471];
const REG_IN_MAX: [u16; 18] = [0x2b, 0x2d, 0x2f, 0x31, 0x33, 0x35, 0x37, 0x554, 0x556, 0x558, 0x55a, 0x55c, 0x55e, 0x560, 0x562, 0x564, 0x570, 0x572];
const REG_IN_MIN: [u16; 18] = [0x2c, 0x2e, 0x30, 0x32, 0x34, 0x36, 0x38, 0x555, 0x557, 0x559, 0x55b, 0x55d, 0x55f, 0x561, 0x563, 0x565, 0x571, 0x573];
/// the fan controls' temperature selects (their monitored temperature: TEMP_MON)
const REG_TEMP_SEL: [u16; 7] = [0x100, 0x200, 0x300, 0x800, 0x900, 0xa00, 0xb00];
/// voltage inputs' mV per 100 register steps
const SCALE: [u16; 15] = [800, 800, 1600, 1600, 800, 800, 800, 1600, 1600, 800, 800, 800, 800, 800, 800];
const SCALE_6798: [u16; 18] = [800, 800, 1600, 1600, 800, 800, 800, 1600, 1600, 1600, 1600, 1600, 800, 800, 800, 800, 1600, 800];

const TEMP_6779: [u16; 2] = [0x27, 0x150];
const SOURCE_6779: [u16; 2] = [0x621, 0x622];
const MON_6779: [u16; 5] = [0x73, 0x75, 0x77, 0x79, 0x7b];
const MON_6792: [u16; 6] = [0x73, 0x75, 0x77, 0x79, 0x7b, 0x7d];
const ALT_6779: [u16; 25] = [0x490, 0x491, 0x492, 0x493, 0x494, 0x495, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x400, 0x401, 0x402, 0x404, 0x405, 0x406, 0x407, 0x408];
const TEMP_6798: [u16; 8] = [0x27, 0x150, 0x670, 0x672, 0x674, 0x676, 0x678, 0x67a];
const SOURCE_6798: [u16; 8] = [0x621, 0x622, 0xc26, 0xc27, 0xc28, 0xc29, 0xc2a, 0xc2b];
const MON_6798: [u16; 7] = [0x73, 0x75, 0x77, 0x79, 0x7b, 0x7d, 0x4a0];
const ALT_6798: [u16; 29] = [0x490, 0x491, 0x492, 0x493, 0x494, 0x495, 0x496, 0, 0, 0, 0, 0, 0x4a2, 0, 0, 0, 0, 0x400, 0x401, 0x402, 0x404, 0x405, 0x406, 0x407, 0x408, 0x419, 0x41a, 0x4f4, 0x4f5];

/// Temperature sources by number ("" none), as the chips name them.
const LABELS_6779: [&str; 32] = [
    "", "SYSTIN", "CPUTIN", "AUXTIN0", "AUXTIN1", "AUXTIN2", "AUXTIN3", "", "SMBUSMASTER 0", "SMBUSMASTER 1", "SMBUSMASTER 2", "SMBUSMASTER 3", "SMBUSMASTER 4", "SMBUSMASTER 5", "SMBUSMASTER 6", "SMBUSMASTER 7",
    "PECI Agent 0", "PECI Agent 1", "PCH_CHIP_CPU_MAX_TEMP", "PCH_CHIP_TEMP", "PCH_CPU_TEMP", "PCH_MCH_TEMP", "PCH_DIM0_TEMP", "PCH_DIM1_TEMP", "PCH_DIM2_TEMP", "PCH_DIM3_TEMP", "BYTE_TEMP", "PECI Agent 0 Calibration",
    "PECI Agent 1 Calibration", "", "", "Virtual_TEMP",
];
const LABELS_6795: [&str; 32] = [
    "", "SYSTIN", "CPUTIN", "AUXTIN0", "AUXTIN1", "AUXTIN2", "AUXTIN3", "", "SMBUSMASTER 0", "SMBUSMASTER 1", "SMBUSMASTER 2", "SMBUSMASTER 3", "SMBUSMASTER 4", "SMBUSMASTER 5", "SMBUSMASTER 6", "SMBUSMASTER 7",
    "PECI Agent 0", "PECI Agent 1", "PCH_CHIP_CPU_MAX_TEMP", "PCH_CHIP_TEMP", "PCH_CPU_TEMP", "PCH_MCH_TEMP", "Agent0 Dimm0", "Agent0 Dimm1", "Agent1 Dimm0", "Agent1 Dimm1", "BYTE_TEMP0", "BYTE_TEMP1",
    "PECI Agent 0 Calibration", "PECI Agent 1 Calibration", "", "Virtual_TEMP",
];
const LABELS_6798: [&str; 32] = [
    "", "SYSTIN", "CPUTIN", "AUXTIN0", "AUXTIN1", "AUXTIN2", "AUXTIN3", "AUXTIN4", "SMBUSMASTER 0", "SMBUSMASTER 1", "Virtual_TEMP", "Virtual_TEMP", "", "", "", "", "PECI Agent 0", "PECI Agent 1",
    "PCH_CHIP_CPU_MAX_TEMP", "PCH_CHIP_TEMP", "PCH_CPU_TEMP", "PCH_MCH_TEMP", "Agent0 Dimm0", "Agent0 Dimm1", "Agent1 Dimm0", "Agent1 Dimm1", "BYTE_TEMP0", "BYTE_TEMP1", "PECI Agent 0 Calibration",
    "PECI Agent 1 Calibration", "", "Virtual_TEMP",
];
const LABELS_6799: [&str; 32] = [
    "", "SYSTIN", "CPUTIN", "AUXTIN0", "AUXTIN1", "AUXTIN2", "AUXTIN3", "AUXTIN4", "SMBUSMASTER 0", "SMBUSMASTER 1", "Virtual_TEMP", "Virtual_TEMP", "", "AUXTIN5", "", "", "PECI Agent 0", "PECI Agent 1",
    "PCH_CHIP_CPU_MAX_TEMP", "PCH_CHIP_TEMP", "PCH_CPU_TEMP", "PCH_MCH_TEMP", "Agent0 Dimm0", "Agent0 Dimm1", "Agent1 Dimm0", "Agent1 Dimm1", "BYTE_TEMP0", "BYTE_TEMP1", "PECI/TSI Agent 0 Calibration",
    "PECI/TSI Agent 1 Calibration", "", "Virtual_TEMP",
];

/// A chip of the family: its name (Linux's), temperature registers and their source
/// selects, the fan controls' temperatures, alternates by source, source labels,
/// the sources it has and which of them are virtual (one per fan control), and
/// voltage inputs with their scales.
pub struct Kind {
    pub name: &'static str,
    temp: &'static [u16],
    source: &'static [u16],
    mon: &'static [u16],
    alt: &'static [u16],
    labels: &'static [&'static str; 32],
    mask: u32,
    virt: u32,
    ins: usize,
    scale: &'static [u16],
    /// its monitor's I/O window may be locked (NCT6791 and later)
    lock: bool,
}

/// The chip by its ID (configuration registers 0x20–0x21; the low 3 bits are the
/// revision).
pub fn kind(id: u16) -> Option<Kind> {
    let k = |name, mon: &'static [u16], labels, mask, virt| Kind { name, temp: &TEMP_6779, source: &SOURCE_6779, mon, alt: &ALT_6779, labels, mask, virt, ins: 15, scale: &SCALE, lock: name != "nct6779" };
    Some(match id & 0xfff8 {
        0xc560 => k("nct6779", &MON_6779, &LABELS_6779, 0x07ff_ff7e, 0),
        0xc800 => k("nct6791", &MON_6779, &LABELS_6779, 0x87ff_ff7e, 0x8000_0000),
        0xc910 => k("nct6792", &MON_6792, &LABELS_6779, 0x9fff_ff7e, 0x8000_0000),
        0xd120 => k("nct6793", &MON_6792, &LABELS_6795, 0xbfff_037e, 0x8000_0000),
        0xd350 => k("nct6795", &MON_6792, &LABELS_6795, 0xbfff_ff7e, 0x8000_0000),
        0xd420 => k("nct6796", &MON_6792, &LABELS_6798, 0xbfff_0ffe, 0x8000_0c00),
        0xd450 => k("nct6797", &MON_6792, &LABELS_6798, 0xbfff_0ffe, 0x8000_0c00),
        0xd428 | 0xd800 => {
            let six = id & 0xfff8 == 0xd428;
            Kind {
                name: if six { "nct6798" } else { "nct6799" },
                temp: &TEMP_6798,
                source: &SOURCE_6798,
                mon: &MON_6798,
                alt: &ALT_6798,
                labels: if six { &LABELS_6798 } else { &LABELS_6799 },
                mask: if six { 0xbfff_0ffe } else { 0xbfff_2ffe },
                virt: 0x8000_0c00,
                ins: if six { 15 } else { 18 },
                scale: &SCALE_6798,
                lock: true,
            }
        }
        _ => return None,
    })
}

/// Is a register 16 bits wide (high byte first)? (the rest are 8 bits)
fn word(r: u16) -> bool {
    matches!(r, 0x150 | 0x153 | 0x155 | 0x402 | 0x63a | 0x63c | 0x63e | 0x640 | 0x642 | 0x64a | 0x64c | 0x73 | 0x75 | 0x77 | 0x79 | 0x7b | 0x7d)
        || r & 0xfff0 == 0x4c0
        || ((0x409..0x419).contains(&r) && r & 1 == 1)
}

/// °C from a temperature register (8-bit ones left-aligned): half degrees from bit 7.
pub fn temp_c(r: u16, v: u16) -> f32 {
    let v = if word(r) { v } else { v << 8 };
    ((v as i16) >> 7) as f32 * 0.5
}

/// Which temperatures the chip has, as Linux's driver finds them: (register, source).
/// First the monitored ones (their sources as `src` reads them), then the fan
/// controls' (virtual sources may repeat there), then alternates for sources not
/// monitored yet; SYSTIN … AUXTIN3 at their own numbers (1–6), the rest after.
/// Returns (hwmon number, register, source).
pub fn temps(k: &Kind, src: &mut dyn FnMut(u16) -> Option<u8>) -> Vec<(u8, u16, u8)> {
    const FIXED: usize = 6;
    const NUM: usize = 10;
    let mut slot: [Option<(u16, u8)>; NUM] = [None; NUM];
    let (mut mask, mut s) = (0u32, FIXED);
    let put = |slot: &mut [Option<(u16, u8)>; NUM], s: &mut usize, reg: u16, x: u8| {
        if (x as usize) <= FIXED {
            slot[x as usize - 1].get_or_insert((reg, x));
        } else if *s < NUM {
            slot[*s] = Some((reg, x));
            *s += 1;
        }
    };
    for (i, &reg) in k.temp.iter().enumerate() {
        let Some(x) = src(k.source[i]).map(|v| v & 0x1f) else { continue };
        if x == 0 || mask & 1 << x != 0 || k.mask & 1 << x == 0 {
            continue;
        }
        mask |= 1 << x;
        put(&mut slot, &mut s, reg, x);
    }
    for (i, &reg) in k.mon.iter().enumerate() {
        let Some(x) = src(REG_TEMP_SEL[i]).map(|v| v & 0x1f) else { continue };
        if x == 0 || k.mask & 1 << x == 0 {
            continue;
        }
        if k.virt & 1 << x == 0 {
            if mask & 1 << x != 0 {
                continue;
            }
            mask |= 1 << x;
        }
        put(&mut slot, &mut s, reg, x);
    }
    for (i, &reg) in k.alt.iter().enumerate() {
        let x = i as u8 + 1;
        if reg == 0 || k.mask & 1 << x == 0 || mask & 1 << x != 0 {
            continue;
        }
        if i >= FIXED && s >= NUM {
            break;
        }
        put(&mut slot, &mut s, reg, x);
    }
    slot.iter().enumerate().filter_map(|(n, t)| t.map(|(r, x)| (n as u8 + 1, r, x))).collect()
}

/// The ISA bus mutex, held (released when dropped).
struct Held(HANDLE);

impl Drop for Held {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.0) };
    }
}

/// The shared ISA bus mutex: created (SYSTEM and administrators, as the tools that
/// take it run), or opened if another tool made it first.
fn isa_mutex() -> HANDLE {
    let sd = super::svc::sd("D:(A;;GA;;;SY)(A;;GA;;;BA)");
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    let name = super::wide(ISA_MUTEX);
    let mut h = unsafe { CreateMutexW(&sa, 0, name.as_ptr()) };
    if h.is_null() && unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
        // (SYNCHRONIZE | MUTEX_MODIFY_STATE)
        h = unsafe { OpenMutexW(0x0010_0001, 0, name.as_ptr()) };
    }
    unsafe { windows_sys::Win32::Foundation::LocalFree(sd as _) };
    h
}

/// A run of register reads through LpcIO: the index port (base + 5), whether the chip
/// is a Nuvoton (banks, 16-bit registers), the bank set now; and what `begin` changed,
/// for `end` to put back: the configuration entered (at this port), the device
/// select and the lock as they were, the index and bank.
struct Io<'a> {
    m: &'a Module,
    port: u64,
    nct: bool,
    bank: u8,
    sio: Option<u64>,
    ldn: Option<u8>,
    lock: Option<u8>,
    saved: Option<(u8, u8)>,
}

impl<'a> Io<'a> {
    /// Begin: with `sio` (the Super I/O's port, for a chip whose window may be
    /// locked), enter its configuration, select the monitor and open the lock; then
    /// note the index (and a Nuvoton's bank). False if something failed (`end` still
    /// undoes what was done).
    fn begin(m: &'a Module, port: u64, nct: bool, sio: Option<u64>) -> (Io<'a>, bool) {
        let mut io = Io { m, port, nct, bank: 0, sio: None, ldn: None, lock: None, saved: None };
        let ok = io.open(sio).is_some();
        (io, ok)
    }

    fn open(&mut self, sio: Option<u64>) -> Option<()> {
        let m = self.m;
        let sin = |r: u64| m.read("ioctl_superio_inb", r).map(|v| v as u8);
        let sout = |r: u64, v: u8| m.call("ioctl_superio_outb", &[r, v as u64], &mut []).map(|_| ());
        if let Some(sp) = sio {
            self.outb(sp, 0x87)?;
            self.sio = Some(sp);
            self.outb(sp, 0x87)?;
            let ldn = sin(SIO_LDSEL)?;
            if ldn != LD_HWM as u8 {
                sout(SIO_LDSEL, LD_HWM as u8)?;
                self.ldn = Some(ldn);
            }
            let l = sin(SIO_HM_LOCK)?;
            if l & 0x10 != 0 {
                sout(SIO_HM_LOCK, l & !0x10)?;
                self.lock = Some(l);
            }
        }
        let index = self.inb(self.port)?;
        if self.nct {
            self.outb(self.port, REG_BANK)?;
            self.bank = self.inb(self.port + 1)?;
        }
        self.saved = Some((index, self.bank));
        Some(())
    }

    /// End: the bank, index, lock and device select back as they were, and the
    /// configuration left.
    fn end(self) {
        let m = self.m;
        let sout = |r: u64, v: u8| m.call("ioctl_superio_outb", &[r, v as u64], &mut []);
        if let Some((index, bank)) = self.saved {
            if self.bank != bank {
                self.outb(self.port, REG_BANK);
                self.outb(self.port + 1, bank);
            }
            self.outb(self.port, index);
        }
        if let Some(sp) = self.sio {
            if let Some(l) = self.lock {
                sout(SIO_HM_LOCK, l);
            }
            if let Some(ldn) = self.ldn {
                sout(SIO_LDSEL, ldn);
            }
            self.outb(sp, 0xaa);
        }
    }

    fn outb(&self, port: u64, v: u8) -> Option<()> {
        self.m.call("ioctl_pio_outb", &[port, v as u64], &mut []).map(|_| ())
    }

    fn inb(&self, port: u64) -> Option<u8> {
        self.m.read("ioctl_pio_inb", port).map(|v| v as u8)
    }

    /// One byte of a register (a Nuvoton's bank in the high byte).
    fn byte(&mut self, r: u16) -> Option<u8> {
        let b = (r >> 8) as u8;
        if b != self.bank {
            self.outb(self.port, REG_BANK)?;
            self.outb(self.port + 1, b)?;
            self.bank = b;
        }
        self.outb(self.port, r as u8)?;
        self.inb(self.port + 1)
    }

    /// A register, 8 or 16 bits (a Nuvoton's word registers).
    fn reg(&mut self, r: u16) -> Option<u16> {
        let v = self.byte(r)? as u16;
        if self.nct && word(r) {
            Some(v << 8 | self.byte(r + 1)? as u16)
        } else {
            Some(v)
        }
    }
}

/// Which family a chip is of.
#[derive(Clone, Copy, PartialEq)]
enum Fam {
    Nct,
    Ite(&'static Ite),
    Fin(&'static Fin),
}

/// A chip found: its family (and a Nuvoton's details), monitor base, the Super I/O's
/// port, and an ITE's GPIO register 0x2C (which inputs are internal).
struct Found {
    fam: Fam,
    nct: Option<Kind>,
    name: &'static str,
    base: u64,
    port: u64,
    gpio2c: u8,
}

/// No register (an item's unused slot).
const NONE: usize = usize::MAX;

/// A reading made of registers: what it is, its number and label, and where its
/// registers are in `Board::regs` (Nuvoton fans: speed, duty, mode; ITE fans:
/// count's low and high bytes, PWM control, duty; Fintek fans: count's high and low
/// bytes, duty; voltages and temperatures: the input), the scale (voltages: V per
/// step; 8-bit ITE fans: the divisor's register 0x0B is in `Board::div`) and limits.
struct Item {
    kind: u8,
    n: u8,
    label: &'static str,
    at: [usize; 4],
    scale: f32,
    lo: f32,
    hi: f32,
}

/// The chip, found: LpcIO (holding its address), the ISA mutex, the index port, the
/// Super I/O's port if the monitor's window may be locked, its family, what to read
/// (and where the registers every fan needs are: ITE's main control and divisors,
/// Fintek's modes, PWM bank and open diodes), and the last readings and when.
pub struct Board {
    m: Module,
    mutex: HANDLE,
    port: u64,
    sio: Option<u64>,
    fam: Fam,
    pub name: &'static str,
    regs: Vec<u16>,
    order: Vec<usize>,
    items: Vec<Item>,
    shared: [usize; 3],
    last: Vec<Reading>,
    at: u64,
}

// (the mutex handle is only used by the service's one thread)
unsafe impl Send for Board {}

impl Drop for Board {
    fn drop(&mut self) {
        super::close(self.mutex);
    }
}

/// Why there's no chip: try again later (no driver or module yet, the bus busy) or
/// not (none here, or one cpuview doesn't read: what was seen).
pub enum Missing {
    Later,
    Never(String),
}

impl Board {
    /// Find the chip (LpcIO.bin from `dir`): a Nuvoton, ITE or Fintek one at 0x2E or
    /// 0x4E, its hardware monitor enabled; then what it has, read once and checked.
    pub fn open(dir: &std::path::Path) -> Result<Board, Missing> {
        let blob = std::fs::read(dir.join("LpcIO.bin")).map_err(|_| Missing::Later)?;
        let m = Module::load(&blob).ok_or(Missing::Later)?;
        let mutex = isa_mutex();
        if mutex.is_null() {
            return Err(Missing::Later);
        }
        let mut b = Board {
            m,
            mutex,
            port: 0,
            sio: None,
            fam: Fam::Nct,
            name: "",
            regs: Vec::new(),
            order: Vec::new(),
            items: Vec::new(),
            shared: [NONE; 3],
            last: Vec::new(),
            at: 0,
        };
        let found = {
            let Some(_held) = b.hold() else { return Err(Missing::Later) };
            b.find()
        };
        let f = found.map_err(Missing::Never)?;
        (b.port, b.name, b.fam) = (f.base + 5, f.name, f.fam);
        b.sio = f.nct.as_ref().is_some_and(|k| k.lock).then_some(f.port);
        let planned = {
            let Some(_held) = b.hold() else { return Err(Missing::Later) };
            let (mut io, ok) = Io::begin(&b.m, b.port, f.fam == Fam::Nct, b.sio);
            let p = if ok { plan(&f, &mut io) } else { Err(String::new()) };
            io.end();
            p
        };
        match planned {
            Ok((regs, items, shared)) => {
                // (a Nuvoton's registers bank by bank)
                let mut order: Vec<usize> = (0..regs.len()).collect();
                order.sort_by_key(|&i| regs[i] >> 8);
                (b.regs, b.order, b.items, b.shared) = (regs, order, items, shared);
            }
            Err(e) if e.is_empty() => return Err(Missing::Later),
            Err(e) => return Err(Missing::Never(e)),
        }
        let v = b.read_now().ok_or(Missing::Later)?;
        if !plausible(&v) {
            let say: Vec<String> = v.iter().filter(|r| matches!(r.kind, B_VOLT | B_TEMP)).map(|r| format!("{}{} {}", r.kind, r.n, r.val)).collect();
            return Err(Missing::Never(format!("{} at {:#x}: readings make no sense: {}", f.name, f.base, say.join(", "))));
        }
        b.last = v;
        Ok(b)
    }

    /// The ISA mutex, waited for up to 250 ms.
    fn hold(&self) -> Option<Held> {
        match unsafe { WaitForSingleObject(self.mutex, 250) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Some(Held(self.mutex)),
            _ => None,
        }
    }

    /// The Super I/O's configuration, at 0x2E then 0x4E: an ITE chip that's already
    /// in it (Gigabyte's second chips stay so), else Nuvoton's and Fintek's key (0x87
    /// twice), else ITE's (0x87 0x01 0x55 0x55, 0xAA at 0x4E); the chip's ID; LpcIO
    /// finds the devices' address ranges (it allows only those); the monitor's base
    /// and whether it's on; then the configuration left as found. (with the mutex)
    fn find(&self) -> Result<Found, String> {
        let m = &self.m;
        let outb = |port: u64, v: u64| m.call("ioctl_pio_outb", &[port, v], &mut []);
        let sin = |r: u64| m.read("ioctl_superio_inb", r).map(|v| v as u8);
        let sout = |r: u64, v: u64| m.call("ioctl_superio_outb", &[r, v], &mut []);
        let inw = |r: u64| m.read("ioctl_superio_inw", r).unwrap_or(0xffff) as u16;
        let mut seen = Vec::new();
        for (slot, port) in [(0u64, 0x2eu64), (1, 0x4e)] {
            if m.call("ioctl_select_slot", &[slot], &mut []).is_none() {
                seen.push(format!("{port:#x}: no access"));
                continue;
            }
            // (which key, if any, was sent: 0 none, 1 Nuvoton/Fintek, 2 ITE)
            let mut key = 0;
            let mut id = inw(SIO_ID);
            let mut fam = ite(id).map(Fam::Ite);
            if fam.is_none() {
                if outb(port, 0x87).and_then(|_| outb(port, 0x87)).is_none() {
                    seen.push(format!("{port:#x}: no access"));
                    continue;
                }
                key = 1;
                id = inw(SIO_ID);
                fam = if kind(id).is_some() {
                    Some(Fam::Nct)
                } else if inw(0x23) == FIN_VENDOR {
                    fin(id).map(Fam::Fin)
                } else {
                    None
                };
            }
            if fam.is_none() {
                outb(port, 0xaa);
                let second = if port == 0x4e { 0xaa } else { 0x55 };
                if [0x87, 0x01, 0x55, second].iter().all(|&b| outb(port, b).is_some()) {
                    key = 2;
                    id = inw(SIO_ID);
                    fam = ite(id).map(Fam::Ite);
                }
            }
            let Some(fam) = fam else {
                seen.push(format!("{port:#x}: chip ID {id:#06x}"));
                if key == 2 {
                    sout(0x02, 0x02);
                    // (at 0x2e ITE's key ends in 0x55, SMSC's: an SMSC chip there took
                    // it and leaves on 0xaa)
                    outb(port, 0xaa);
                }
                continue;
            };
            let (name, ld) = match fam {
                Fam::Nct => (kind(id).map_or("", |k| k.name), LD_HWM),
                Fam::Ite(c) => (c.name, ITE_LD),
                Fam::Fin(c) => (c.name, FIN_LD),
            };
            let ldn0 = sin(SIO_LDSEL);
            let mut got = Err(format!("{port:#x}: {name}, no address ranges"));
            if m.call("ioctl_find_bars", &[], &mut []).is_some() && sout(SIO_LDSEL, ld).is_some() {
                let base = m.read("ioctl_superio_inw", SIO_ADDR).unwrap_or(0) & 0xfff8;
                let on = sin(SIO_ENABLE).is_some_and(|v| v & 1 != 0);
                let mut gpio2c = 0;
                if matches!(fam, Fam::Ite(_)) && sout(SIO_LDSEL, ITE_GPIO_LD).is_some() {
                    gpio2c = sin(0x2c).unwrap_or(0);
                }
                got = if base >= 0x100 && on {
                    Ok(Found { fam, nct: kind(id).filter(|_| fam == Fam::Nct), name, base, port, gpio2c })
                } else {
                    Err(format!("{port:#x}: {name} with its monitor at {base:#x}, {}", if on { "on" } else { "off" }))
                };
            }
            // (the device select back; the configuration left only if this entered it)
            if let Some(l) = ldn0 {
                sout(SIO_LDSEL, l as u64);
            }
            match key {
                1 => {
                    outb(port, 0xaa);
                }
                // (Gigabyte's second chips stay in configuration mode: never left)
                2 if !matches!(fam, Fam::Ite(c) if c.f & NOCONF != 0) => {
                    sout(0x02, 0x02);
                }
                _ => {}
            }
            match got {
                Ok(f) => return Ok(f),
                Err(e) => seen.push(e),
            }
        }
        Err(seen.join("; "))
    }

    /// Read every register now (with the mutex) and make the readings.
    fn read_now(&mut self) -> Option<Vec<Reading>> {
        let mut v = vec![0u16; self.regs.len()];
        {
            let _held = self.hold()?;
            let (mut io, mut ok) = Io::begin(&self.m, self.port, self.fam == Fam::Nct, self.sio);
            for &i in self.order.iter().take(if ok { usize::MAX } else { 0 }) {
                match io.reg(self.regs[i]) {
                    Some(x) => v[i] = x,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            io.end();
            if !ok {
                return None;
            }
        }
        let byte = |i: usize| if i == NONE { 0 } else { v[i] as u8 };
        let s8 = |i: usize| byte(i) as i8 as f32;
        let mut out = vec![Reading { kind: B_CHIP, label: self.name.into(), ..Reading::default() }];
        for it in &self.items {
            let mut r = Reading { kind: it.kind, n: it.n, label: it.label.into(), pwm: -1.0, en: -1, ..Reading::default() };
            match (it.kind, self.fam) {
                (B_FAN, Fam::Nct) => {
                    let (duty, mode) = (byte(it.at[1]), (byte(it.at[2]) >> 4) & 7);
                    r.val = v[it.at[0]] as f32;
                    r.pwm = duty as f32 / 255.0;
                    r.en = if mode == 0 && duty == 255 { 0 } else { mode as i8 + 1 };
                }
                (B_FAN, Fam::Ite(c)) => {
                    let wide = it.at[1] != NONE;
                    let count = if wide { (byte(it.at[1]) as u16) << 8 | byte(it.at[0]) as u16 } else { byte(it.at[0]) as u16 };
                    let rpm = ite_rpm(count, wide, ite_div(byte(self.shared[1]), it.n));
                    r.val = rpm.unwrap_or(-1.0);
                    if it.at[2] != NONE {
                        (r.pwm, r.en) = ite_pwm(c, it.n, byte(self.shared[0]), byte(it.at[2]), byte(it.at[3]));
                    }
                }
                (B_FAN, Fam::Fin(c)) => {
                    r.val = fin_rpm((byte(it.at[0]) as u16) << 8 | byte(it.at[1]) as u16);
                    (r.pwm, r.en) = fin_pwm(c, it.n - 1, byte(self.shared[0]), byte(self.shared[1]), byte(it.at[2]));
                }
                (B_VOLT, _) => (r.val, r.lo, r.hi) = (v[it.at[0]] as f32 * it.scale, it.lo, it.hi),
                (_, Fam::Nct) => r.val = temp_c(self.regs[it.at[0]], v[it.at[0]]),
                (_, Fam::Fin(c)) => {
                    // (an open diode: no reading)
                    let open = byte(self.shared[2]) & (1 << (c.t0 + it.n - 1)) != 0;
                    r.val = if open { -128.0 } else { s8(it.at[0]) };
                }
                _ => r.val = s8(it.at[0]),
            }
            out.push(r);
        }
        Some(out)
    }

    /// The readings, at most 1.5 s old (the last ones if the bus is busy).
    pub fn read(&mut self) -> Vec<Reading> {
        let now = unsafe { GetTickCount64() };
        if now >= self.at + EVERY {
            self.at = now;
            if let Some(v) = self.read_now() {
                self.last = v;
            }
        }
        self.last.clone()
    }
}

/// What to read each time, by the chip's family (its configuration read now, once):
/// the registers, the readings made of them and the registers every fan needs; an
/// error says why a chip isn't read (empty: try again later).
type Plan = (Vec<u16>, Vec<Item>, [usize; 3]);

fn plan(f: &Found, io: &mut Io) -> Result<Plan, String> {
    let later = || String::new();
    let mut regs = Vec::new();
    let mut add = |r: u16| {
        regs.push(r);
        regs.len() - 1
    };
    let mut items = Vec::new();
    let mut shared = [NONE; 3];
    let item = |kind, n, label, at, scale| Item { kind, n, label, at, scale, lo: 0.0, hi: 0.0 };
    match f.fam {
        Fam::Nct => {
            let k = f.nct.as_ref().ok_or_else(later)?;
            for i in 0..REG_FAN.len() {
                items.push(item(B_FAN, i as u8 + 1, "", [add(REG_FAN[i]), add(REG_PWM[i]), add(REG_FAN_MODE[i]), NONE], 1.0));
            }
            for i in 0..k.ins {
                let scale = k.scale[i] as f32 / 100_000.0;
                let lo = io.reg(REG_IN_MIN[i]).ok_or_else(later)? as f32 * scale;
                let hi = io.reg(REG_IN_MAX[i]).ok_or_else(later)? as f32 * scale;
                items.push(Item { lo, hi, ..item(B_VOLT, i as u8, "", [add(REG_IN[i]), NONE, NONE, NONE], scale) });
            }
            for (n, reg, x) in temps(k, &mut |r| io.reg(r).map(|v| v as u8)) {
                let label = k.labels[x as usize];
                if !label.is_empty() {
                    items.push(item(B_TEMP, n, label, [add(reg), NONE, NONE, NONE], 1.0));
                }
            }
        }
        Fam::Ite(c) => {
            let mut r = |x: u16| io.reg(x).map(|v| v as u8).ok_or_else(later);
            // (the chip's own check: its vendor register, and the reset bit clear)
            if r(ITE_VENDOR)? != 0x90 || r(0x00)? & 0x80 != 0 {
                return Err(format!("{} at {:#x}: not answering as one", c.name, f.base));
            }
            let (main, fan16) = (r(ITE_MAIN)?, r(ITE_FAN16)?);
            let (en, extra) = (r(ITE_TEMP_EN)?, if c.f & SIX_TEMP != 0 { r(ITE_TEMP_EXTRA)? } else { 0 });
            shared = [add(ITE_MAIN), add(ITE_DIV), NONE];
            for (n, wide) in ite_fans(c, main, fan16) {
                let i = n as usize - 1;
                let hi = if wide { add(ITE_FAN_HI[i]) } else { NONE };
                let (ctl, duty) = if i < c.pwms { (add(ITE_PWM_CTL[i]), if c.f & NEWER_PWM != 0 { add(ITE_PWM_DUTY[i]) } else { NONE }) } else { (NONE, NONE) };
                items.push(item(B_FAN, n, "", [add(ITE_FAN_LO[i]), hi, ctl, duty], 1.0));
            }
            for (n, reg, mul, label) in ite_ins(c, f.gpio2c) {
                items.push(item(B_VOLT, n, label, [add(reg), NONE, NONE, NONE], c.lsb * mul / 1000.0));
            }
            for (n, reg) in ite_temps(c, en, extra) {
                items.push(item(B_TEMP, n, "", [add(reg), NONE, NONE, NONE], 1.0));
            }
        }
        Fam::Fin(c) => {
            let start = io.reg(FIN_START).ok_or_else(later)? as u8;
            if start & 4 != 0 || start & 3 == 0 {
                return Err(format!("{} at {:#x}: its monitor is off", c.name, f.base));
            }
            shared = [add(FIN_MODES), if c.banked { add(FIN_BANK) } else { NONE }, add(FIN_OPEN)];
            if start & 2 != 0 {
                for i in 0..c.fans as u16 {
                    let b = 0xa0 + 16 * i;
                    items.push(item(B_FAN, i as u8 + 1, "", [add(b), add(b + 1), add(b + 3), NONE], 1.0));
                }
            }
            if start & 1 != 0 {
                for n in (0..11u8).filter(|&n| c.ins & (1 << n) != 0) {
                    items.push(item(B_VOLT, n, "", [add(0x20 + n as u16), NONE, NONE, NONE], 0.008));
                }
                for i in 0..c.temps {
                    items.push(item(B_TEMP, i + 1, "", [add(0x70 + 2 * (c.t0 + i) as u16), NONE, NONE, NONE], 1.0));
                }
            }
        }
    }
    Ok((regs, items, shared))
}

/// Does a first reading look like a working chip: a voltage a board's rails could
/// have and a temperature a computer's insides could have? (a wrong address, or a
/// window that's shut, reads all ones or zeros)
pub fn plausible(v: &[Reading]) -> bool {
    v.iter().any(|r| r.kind == B_VOLT && (0.2..=2.5).contains(&r.val)) && v.iter().any(|r| r.kind == B_TEMP && (5.0..=110.0).contains(&r.val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips() {
        assert_eq!(kind(0xd42b).map(|k| k.name), Some("nct6798"));
        assert_eq!(kind(0xd802).map(|k| (k.name, k.ins)), Some(("nct6799", 18)));
        assert_eq!(kind(0xc562).map(|k| k.name), Some("nct6779"));
        assert!(kind(0x8686).is_none() && kind(0xffff).is_none());
        assert!(word(0x150) && word(0x4c0) && word(0x4ce) && word(0x73) && word(0x409));
        assert!(!word(0x27) && !word(0x480) && !word(0x670) && !word(0x40a));
    }

    #[test]
    fn decoding() {
        // (8-bit 45 °C; 16-bit 45.5 °C; below zero)
        assert_eq!(temp_c(0x27, 45), 45.0);
        assert_eq!(temp_c(0x150, 45 << 8 | 0x80), 45.5);
        assert_eq!(temp_c(0x27, 0xfb), -5.0);
    }

    /// The temperatures an NCT6798 shows, from a board's source selects: SYSTIN at
    /// 0x27, CPUTIN at 0x150, the fan controls on CPUTIN (a repeat), PECI 0 and two
    /// virtual ones, alternates for the rest.
    #[test]
    fn which_temps() {
        let k = kind(0xd428).unwrap();
        let src = |r: u16| -> Option<u8> {
            Some(match r {
                0x621 => 1,
                0x622 => 2,
                0xc26 => 16,
                0x100 | 0x200 => 2,
                0x300 => 31,
                0x800 => 31,
                0x900 => 3,
                _ => 0,
            })
        };
        let t = temps(&k, &mut |r| src(r));
        // fixed: SYSTIN(1), CPUTIN(2), AUXTIN0(3: fan control 5's), AUXTIN1–3
        // (alternates 0x493–0x495); then PECI 0, the two virtual ones, AUXTIN4
        assert_eq!(&t[..6], &[(1, 0x27, 1), (2, 0x150, 2), (3, 0x7b, 3), (4, 0x493, 4), (5, 0x494, 5), (6, 0x495, 6)]);
        assert_eq!(&t[6..], &[(7, 0x670, 16), (8, 0x77, 31), (9, 0x79, 31), (10, 0x496, 7)]);
    }

    #[test]
    fn sanity() {
        let r = |kind, n, val| Reading { kind, n, val, ..Reading::default() };
        assert!(plausible(&[r(B_VOLT, 0, 1.25), r(B_TEMP, 1, 31.0)]));
        assert!(!plausible(&[r(B_VOLT, 0, 2.04), r(B_TEMP, 1, -1.0)]));
        assert!(plausible(&[r(B_VOLT, 0, 3.3), r(B_VOLT, 1, 1.2), r(B_TEMP, 1, 31.0)]));
        assert!(!plausible(&[r(B_VOLT, 0, 0.0), r(B_TEMP, 1, 31.0)]));
        assert!(!plausible(&[r(B_VOLT, 0, 6.12), r(B_TEMP, 1, 31.0)]));
    }
}

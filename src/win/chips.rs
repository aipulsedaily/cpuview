//! Board monitoring chips other than Nuvoton's (win/board.rs reads them): ITE's
//! IT87xx / IT86xx environment controllers and Fintek's F718xx / F81xxx hardware
//! monitors. What each has and how its registers read, from ITE's and Fintek's
//! register facts as Linux's it87 and f71882fg drivers know them (the code is
//! cpuview's own). Only reads: chips whose reading would take a write (IT8622E
//! switches its SMBus off around each access) aren't here, nor chips those facts
//! don't cover (IT8686E, IT8688E, IT8689E …) or F71858 (another temperature format).

/// ITE: environment controller's logical device; the chip's own check registers
pub const ITE_LD: u64 = 0x04;
pub const ITE_GPIO_LD: u64 = 0x07;

// ITE chip features
pub const NEWER_PWM: u16 = 1;
pub const FAN16_CFG: u16 = 2;
pub const IN7_INT: u16 = 4;
pub const AVCC3: u16 = 8;
pub const SIX_TEMP: u16 = 16;
pub const ONOFF: u16 = 32;
/// the BIOS leaves it in configuration mode: never enter or leave it (Gigabyte's
/// second chips)
pub const NOCONF: u16 = 64;
/// in3 reads +5 V (not +3.3 V) when internal
pub const VIN3_5V: u16 = 128;
pub const NO_IN56: u16 = 256;

/// An ITE chip: ID, name (Linux's), ADC step (mV), fans, PWM outputs, features.
#[derive(PartialEq)]
pub struct Ite {
    pub id: u16,
    pub name: &'static str,
    pub lsb: f32,
    pub fans: usize,
    pub pwms: usize,
    pub f: u16,
}

const ITE: [Ite; 19] = [
    Ite { id: 0x8716, name: "it8716", lsb: 16.0, fans: 5, pwms: 3, f: FAN16_CFG | ONOFF },
    Ite { id: 0x8726, name: "it8716", lsb: 16.0, fans: 5, pwms: 3, f: FAN16_CFG | ONOFF },
    Ite { id: 0x8718, name: "it8718", lsb: 16.0, fans: 5, pwms: 3, f: FAN16_CFG | ONOFF },
    Ite { id: 0x8720, name: "it8720", lsb: 16.0, fans: 5, pwms: 3, f: FAN16_CFG | ONOFF },
    Ite { id: 0x8721, name: "it8721", lsb: 12.0, fans: 5, pwms: 3, f: NEWER_PWM | FAN16_CFG | IN7_INT | ONOFF },
    Ite { id: 0x8728, name: "it8728", lsb: 12.0, fans: 5, pwms: 3, f: NEWER_PWM | IN7_INT | ONOFF },
    Ite { id: 0x8732, name: "it8732", lsb: 10.9, fans: 4, pwms: 4, f: NEWER_PWM | IN7_INT | ONOFF },
    Ite { id: 0x8771, name: "it8771", lsb: 12.0, fans: 3, pwms: 3, f: NEWER_PWM | IN7_INT | ONOFF },
    Ite { id: 0x8772, name: "it8772", lsb: 12.0, fans: 3, pwms: 3, f: NEWER_PWM | IN7_INT | ONOFF },
    Ite { id: 0x8781, name: "it8781", lsb: 16.0, fans: 3, pwms: 3, f: FAN16_CFG | ONOFF },
    Ite { id: 0x8782, name: "it8782", lsb: 16.0, fans: 3, pwms: 3, f: FAN16_CFG | ONOFF },
    Ite { id: 0x8783, name: "it8783", lsb: 16.0, fans: 3, pwms: 3, f: FAN16_CFG | ONOFF },
    Ite { id: 0x8786, name: "it8786", lsb: 12.0, fans: 3, pwms: 3, f: NEWER_PWM | IN7_INT | ONOFF },
    Ite { id: 0x8790, name: "it8790", lsb: 12.0, fans: 3, pwms: 3, f: NEWER_PWM | IN7_INT | ONOFF | NOCONF },
    Ite { id: 0x8733, name: "it8792", lsb: 10.9, fans: 3, pwms: 3, f: NEWER_PWM | IN7_INT | ONOFF | NOCONF },
    Ite { id: 0x8695, name: "it87952", lsb: 10.9, fans: 3, pwms: 3, f: NEWER_PWM | IN7_INT | ONOFF | NOCONF },
    Ite { id: 0x8603, name: "it8603", lsb: 12.0, fans: 3, pwms: 3, f: NEWER_PWM | IN7_INT | AVCC3 | NO_IN56 },
    Ite { id: 0x8620, name: "it8620", lsb: 12.0, fans: 6, pwms: 6, f: NEWER_PWM | IN7_INT | SIX_TEMP | ONOFF | VIN3_5V },
    Ite { id: 0x8628, name: "it8628", lsb: 12.0, fans: 6, pwms: 6, f: NEWER_PWM | IN7_INT | SIX_TEMP | ONOFF | VIN3_5V },
];

pub fn ite(id: u16) -> Option<&'static Ite> {
    let id = if id == 0x8623 { 0x8603 } else { id };
    ITE.iter().find(|c| c.id == id)
}

/// ITE fan registers: tachometer low and high bytes; PWM control and (newer chips)
/// duty registers.
pub const ITE_FAN_LO: [u16; 6] = [0x0d, 0x0e, 0x0f, 0x80, 0x82, 0x4c];
pub const ITE_FAN_HI: [u16; 6] = [0x18, 0x19, 0x1a, 0x81, 0x83, 0x4d];
pub const ITE_PWM_CTL: [u16; 6] = [0x15, 0x16, 0x17, 0x7f, 0xa7, 0xaf];
pub const ITE_PWM_DUTY: [u16; 6] = [0x63, 0x6b, 0x73, 0x7b, 0xa3, 0xab];
/// fan main control (PWM mode and tachometer enables of fans 1–3), 16-bit and fan 4–6
/// enables, fan divisors, temperature types, extra temperatures' use
pub const ITE_MAIN: u16 = 0x13;
pub const ITE_FAN16: u16 = 0x0c;
pub const ITE_DIV: u16 = 0x0b;
pub const ITE_TEMP_EN: u16 = 0x51;
pub const ITE_TEMP_EXTRA: u16 = 0x77;
/// the chip's own check: its vendor register
pub const ITE_VENDOR: u16 = 0x58;

/// Which ITE fans exist (1-based numbers, and whether each counts in 16 bits),
/// from the fan main control and the 16-bit register.
pub fn ite_fans(c: &Ite, main: u8, fan16: u8) -> Vec<(u8, bool)> {
    let mut v = Vec::new();
    for n in 0..c.fans {
        let on = match n {
            0..=2 => main & (0x10 << n) != 0,
            3 => fan16 & 0x10 != 0,
            4 => fan16 & 0x20 != 0,
            _ => fan16 & 0x04 != 0,
        };
        if on {
            // (FAN16_CFG chips: fans 1–3 count in 16 bits only where switched on)
            let wide = n >= 3 || c.f & FAN16_CFG == 0 || fan16 & (1 << n) != 0;
            v.push((n as u8 + 1, wide));
        }
    }
    v
}

/// RPM from an ITE tachometer count: 16-bit (0: invalid, 0xFFFF: stopped) or 8-bit
/// with its divisor (0xFF: stopped).
pub fn ite_rpm(count: u16, wide: bool, div: u32) -> Option<f32> {
    match (wide, count) {
        (_, 0) => None,
        (true, 0xffff) | (false, 0xff) => Some(0.0),
        (true, c) => Some(675_000.0 / c as f32),
        (false, c) => Some(1_350_000.0 / (c as u32 * div.max(1)) as f32),
    }
}

/// An 8-bit fan's divisor (fans 1–3) from the divisor register.
pub fn ite_div(reg: u8, n: u8) -> u32 {
    match n {
        1 => 1 << (reg & 7),
        2 => 1 << ((reg >> 3) & 7),
        _ => {
            if reg & 0x40 != 0 {
                8
            } else {
                2
            }
        }
    }
}

/// An ITE fan's duty (0..1, -1: not readable) and control mode as Linux's
/// pwmN_enable (0 full speed / on-off, 1 manual, 2 automatic), from the main
/// control, the PWM control and (newer chips) the duty register.
pub fn ite_pwm(c: &Ite, n: u8, main: u8, ctl: u8, duty: u8) -> (f32, i8) {
    let i = n as usize - 1;
    let newer = c.f & NEWER_PWM != 0;
    let onoff = c.f & ONOFF != 0;
    let (d, max) = if newer { (duty as f32 / 255.0, duty == 0xff) } else { ((ctl & 0x7f) as f32 / 127.0, ctl & 0x7f == 0x7f) };
    if onoff && i < 3 && main & (1 << i) == 0 {
        return (1.0, 0);
    }
    if ctl & 0x80 != 0 {
        // (automatic: newer chips still show the duty the chip set)
        return (if newer { d } else { -1.0 }, 2);
    }
    if (!onoff || i >= 3) && max {
        return (1.0, 0);
    }
    (d, 1)
}

/// The ITE voltage inputs: (input, register, scale ×, label where Linux has one).
/// `gpio2c`: GPIO register 0x2C (bit 0: in3 internal, bit 1: in7 internal).
pub fn ite_ins(c: &Ite, gpio2c: u8) -> Vec<(u8, u16, f32, &'static str)> {
    let new_adc = c.lsb < 16.0;
    let mut v = Vec::new();
    for n in 0u8..=9 {
        if (n == 5 || n == 6) && c.f & NO_IN56 != 0 || n == 9 && c.f & AVCC3 == 0 {
            continue;
        }
        let scaled = match n {
            3 => (new_adc || c.name.starts_with("it878")) && gpio2c & 1 != 0,
            7 => (new_adc && c.f & IN7_INT != 0) || (c.name.starts_with("it878") && gpio2c & 2 != 0),
            8 => new_adc,
            9 => true,
            _ => false,
        };
        let label = match (n, scaled) {
            (3, true) => {
                if c.f & VIN3_5V != 0 || !new_adc {
                    "+5V"
                } else {
                    "+3.3V"
                }
            }
            (7, true) => {
                if new_adc {
                    "3VSB"
                } else {
                    "5VSB"
                }
            }
            (8, _) => "Vbat",
            (9, _) => "+3.3V",
            _ => "",
        };
        let reg = if n == 9 { 0x2f } else { 0x20 + n as u16 };
        v.push((n, reg, if scaled { 2.0 } else { 1.0 }, label));
    }
    v
}

/// The ITE temperatures that are on: (number, register), from the type register
/// (diode, thermistor or PECI / TSI per input; off otherwise) and on six-temperature
/// chips the extra inputs' use.
pub fn ite_temps(c: &Ite, en: u8, extra: u8) -> Vec<(u8, u16)> {
    let mut v = Vec::new();
    for i in 0..3u8 {
        let peci = (en >> 6) == i + 1;
        if peci || en & (1 << i) != 0 || en & (8 << i) != 0 {
            v.push((i + 1, 0x29 + i as u16));
        }
    }
    if c.f & SIX_TEMP != 0 {
        for k in 0..3u8 {
            if (extra >> (2 * k)) & 3 >= 2 {
                v.push((k + 4, 0x2c + k as u16));
            }
        }
    }
    v
}

/// A Fintek chip: ID, name (Linux's), fans, temperatures, its first temperature
/// channel, voltage inputs (bit n: inN), and whether its PWM registers are banked
/// (register 0x9F bit 7).
#[derive(PartialEq)]
pub struct Fin {
    pub id: u16,
    pub name: &'static str,
    pub fans: u8,
    pub temps: u8,
    pub t0: u8,
    pub ins: u16,
    pub banked: bool,
}

const FIN: [Fin; 15] = [
    Fin { id: 0x0901, name: "f71808e", fans: 3, temps: 2, t0: 1, ins: 0x1bf, banked: true },
    Fin { id: 0x1001, name: "f71808a", fans: 2, temps: 2, t0: 1, ins: 0x18f, banked: true },
    Fin { id: 0x0601, name: "f71862fg", fans: 3, temps: 3, t0: 1, ins: 0x1ff, banked: false },
    Fin { id: 0x1106, name: "f71868a", fans: 3, temps: 3, t0: 1, ins: 0x3ff, banked: false },
    Fin { id: 0x0814, name: "f71869", fans: 3, temps: 3, t0: 1, ins: 0x1ff, banked: true },
    Fin { id: 0x1007, name: "f71869a", fans: 3, temps: 3, t0: 1, ins: 0x1ff, banked: true },
    Fin { id: 0x0541, name: "f71882fg", fans: 4, temps: 3, t0: 1, ins: 0x1ff, banked: false },
    Fin { id: 0x0723, name: "f71889fg", fans: 3, temps: 3, t0: 1, ins: 0x1ff, banked: true },
    Fin { id: 0x0909, name: "f71889ed", fans: 3, temps: 3, t0: 1, ins: 0x1ff, banked: true },
    Fin { id: 0x1005, name: "f71889a", fans: 3, temps: 3, t0: 1, ins: 0x1ff, banked: true },
    Fin { id: 0x0581, name: "f8000", fans: 3, temps: 3, t0: 0, ins: 0x007, banked: false },
    Fin { id: 0x1210, name: "f81768d", fans: 3, temps: 3, t0: 1, ins: 0x7ff, banked: false },
    Fin { id: 0x0704, name: "f81865f", fans: 2, temps: 2, t0: 1, ins: 0x07f, banked: false },
    Fin { id: 0x1010, name: "f81866a", fans: 3, temps: 3, t0: 0, ins: 0x0ff, banked: false },
    Fin { id: 0x1502, name: "f81866a", fans: 3, temps: 3, t0: 0, ins: 0x0ff, banked: false },
];

pub fn fin(id: u16) -> Option<&'static Fin> {
    FIN.iter().find(|c| c.id == id)
}

/// Fintek: its vendor ID (configuration registers 0x23–0x24), the monitor's logical
/// device; the monitor's start register (bit 0 temperatures and voltages on, bit 1
/// fans on, bit 2 powered down), fan modes, the PWM bank, open temperature diodes
pub const FIN_VENDOR: u16 = 0x1934;
pub const FIN_LD: u64 = 0x04;
pub const FIN_START: u16 = 0x01;
pub const FIN_MODES: u16 = 0x96;
pub const FIN_BANK: u16 = 0x9f;
pub const FIN_OPEN: u16 = 0x6f;

/// RPM from a Fintek tachometer count (0 and 0x0FFF: stopped).
pub fn fin_rpm(count: u16) -> f32 {
    if count == 0 || count >= 0x0fff {
        0.0
    } else {
        1_500_000.0 / count as f32
    }
}

/// A Fintek fan's duty (0..1, -1: not readable: speed mode, or the PWM registers'
/// other bank) and mode (1 manual, 2 automatic; F8000: -1), from the modes register.
pub fn fin_pwm(c: &Fin, i: u8, modes: u8, bank: u8, duty: u8) -> (f32, i8) {
    let field = (modes >> (2 * i)) & 3;
    let readable = field & 1 != 0 && !(c.banked && bank & 0x80 != 0);
    let d = if readable { duty as f32 / 255.0 } else { -1.0 };
    let en = if c.name == "f8000" { -1 } else if field & 2 != 0 { 1 } else { 2 };
    (d, en)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ite_chips() {
        assert_eq!(ite(0x8628).map(|c| c.fans), Some(6));
        assert_eq!(ite(0x8623).map(|c| c.name), Some("it8603"));
        assert!(ite(0x8622).is_none() && ite(0x8688).is_none());
        // fans 1 and 3 on (main control), 4 on, 6 on (six-fan chip)
        let c = ite(0x8628).unwrap();
        assert_eq!(ite_fans(c, 0x50, 0x14), vec![(1, true), (3, true), (4, true), (6, true)]);
        // an IT8720 with fan 2 still counting in 8 bits
        let c = ite(0x8720).unwrap();
        assert_eq!(ite_fans(c, 0x30, 0x01), vec![(1, true), (2, false)]);
    }

    #[test]
    fn ite_values() {
        assert_eq!(ite_rpm(675, true, 0), Some(1000.0));
        assert_eq!(ite_rpm(0xffff, true, 0), Some(0.0));
        assert_eq!(ite_rpm(0, true, 0), None);
        assert_eq!(ite_rpm(135, false, 4), Some(2500.0));
        assert_eq!(ite_div(0x5a, 1), 4);
        assert_eq!(ite_div(0x5a, 2), 8);
        assert_eq!(ite_div(0x5a, 3), 8);
        let c = ite(0x8628).unwrap();
        // fan 1: PWM mode, automatic, duty 0x80; fan 2: on/off mode; manual 0xFF
        assert_eq!(ite_pwm(c, 1, 0x07, 0x80, 0x80), (128.0 / 255.0, 2));
        assert_eq!(ite_pwm(c, 2, 0x05, 0x00, 0x40), (1.0, 0));
        assert_eq!(ite_pwm(c, 4, 0x07, 0x00, 0xff), (1.0, 0));
        assert_eq!(ite_pwm(c, 1, 0x07, 0x00, 0x33), (0.2, 1));
    }

    #[test]
    fn ite_inputs() {
        let c = ite(0x8628).unwrap();
        let v = ite_ins(c, 0x01);
        assert_eq!(v.iter().find(|x| x.0 == 3).map(|x| (x.2, x.3)), Some((2.0, "+5V")));
        assert_eq!(v.iter().find(|x| x.0 == 7).map(|x| (x.2, x.3)), Some((2.0, "3VSB")));
        assert_eq!(v.iter().find(|x| x.0 == 8).map(|x| (x.2, x.3)), Some((2.0, "Vbat")));
        assert!(v.iter().all(|x| x.0 < 9));
        let c = ite(0x8603).unwrap();
        let v = ite_ins(c, 0);
        assert!(v.iter().all(|x| x.0 != 5 && x.0 != 6) && v.iter().any(|x| x.0 == 9 && x.1 == 0x2f));
        // temperatures: 1 a diode, 2 a thermistor, 3 off; extra 4 and 6 temperatures
        assert_eq!(ite_temps(ite(0x8628).unwrap(), 0x11, 0x23), vec![(1, 0x29), (2, 0x2a), (4, 0x2c), (6, 0x2e)]);
        // PECI on input 3
        assert_eq!(ite_temps(ite(0x8728).unwrap(), 0xc0, 0), vec![(3, 0x2b)]);
    }

    #[test]
    fn fintek() {
        assert_eq!(fin(0x0541).map(|c| c.fans), Some(4));
        assert!(fin(0x0507).is_none());
        assert_eq!(fin_rpm(1500), 1000.0);
        assert_eq!(fin_rpm(0x0fff), 0.0);
        let c = fin(0x0723).unwrap();
        // fan 0 manual duty mode, fan 1 automatic duty mode, then automatic speed
        // mode; the other bank
        assert_eq!(fin_pwm(c, 0, 0x07, 0, 0x80), (128.0 / 255.0, 1));
        assert_eq!(fin_pwm(c, 1, 0x07, 0, 0x80), (128.0 / 255.0, 2));
        assert_eq!(fin_pwm(c, 1, 0x03, 0, 0x80), (-1.0, 2));
        assert_eq!(fin_pwm(c, 0, 0x03, 0x80, 0x80), (-1.0, 1));
    }
}

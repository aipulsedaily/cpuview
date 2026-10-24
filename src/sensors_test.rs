//! Fixture tests for view 5: fake /sys trees of four kinds of machine (this ASUS
//! B850 desktop, an Intel ThinkPad with a battery, a Raspberry Pi with only a thermal
//! zone, a virtual machine with nothing), counter wraparound and the plain names.

use super::*;
use crate::screen::Screen;
use crate::ui::{App, PG_SENS};

/// A fake filesystem root under the temp dir, removed when dropped.
struct Fx(String);

impl Fx {
    fn new(name: &str) -> Fx {
        // (unique: tests run in parallel and build the same machines)
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = format!(
            "{}/cpuview-fx-{name}-{}-{n}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Fx(d)
    }
    /// Files under a directory: (name, content) pairs.
    fn dir(&self, d: &str, files: &[(&str, &str)]) -> &Fx {
        let full = format!("{}{d}", self.0);
        std::fs::create_dir_all(&full).unwrap();
        for (f, v) in files {
            std::fs::write(format!("{full}/{f}"), format!("{v}\n")).unwrap();
        }
        self
    }
    /// A symlink at `at` to `to` (both inside the root).
    fn link(&self, at: &str, to: &str) -> &Fx {
        let to = format!("{}{to}", self.0);
        std::fs::create_dir_all(&to).unwrap();
        let _ = std::os::unix::fs::symlink(&to, format!("{}{at}", self.0));
        self
    }
    fn sens(&self) -> Sens {
        let mut s = Sens::at(&self.0);
        s.detect(1_000_000_000);
        s
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn find<'a>(s: &'a Sens, raw: &str) -> &'a Rd {
    s.rd.iter()
        .find(|r| r.raw == raw)
        .unwrap_or_else(|| panic!("no reading {raw}"))
}

/// The page drawn at w × h for these sensors (as text).
fn page(s: Sens, w: usize, h: usize) -> String {
    let mut sm = crate::sys::Sampler::new_for_test(&(0..8).collect::<Vec<_>>());
    sm.t.groups = (0..8).map(|c| vec![c]).collect();
    sm.t.ccd = vec![0; 8];
    sm.t.ndom = 1;
    sm.t.rank = vec![-1; 8];
    let mut a = App::new(sm, crate::theme::load("default").unwrap());
    a.z = s;
    a.page = PG_SENS;
    a.first_size(w, h);
    a.sens_retarget(false);
    let mut scr = Screen::new(w, h);
    a.draw(&mut scr, true);
    a.draw(&mut scr, false);
    let mut t = String::new();
    for y in 0..h {
        for x in 0..w {
            t.push(scr.buf[y * w + x].ch);
        }
        t.push('\n');
    }
    assert!(!t.contains("NaN") && !t.contains("inf"), "{w}x{h}\n{t}");
    t
}

/// The user's machine: ASUS ProArt B850 (nct6799), Ryzen 9 9950X, 2 DDR5 sticks,
/// an NVMe SSD, the iGPU, two Realtek NICs; RAPL present but root-only.
fn asus_b850() -> Fx {
    let fx = Fx::new("b850");
    let pci = "/sys/devices/pci0000:00";
    fx.dir(
        "/sys/class/dmi/id",
        &[
            ("board_vendor", "ASUSTeK COMPUTER INC."),
            ("board_name", "ProArt B850-CREATOR WIFI NEO"),
            ("sys_vendor", "ASUS"),
            ("product_name", "System Product Name"),
        ],
    )
    .dir("/proc", &[("cpuinfo", "processor\t: 0\nvendor_id\t: AuthenticAMD")])
    .dir("/proc/sys/kernel", &[("perf_event_paranoid", "4")])
    // network chips: the PHY sits under the NIC, whose net/ names the interface
    .dir("/sys/class/hwmon/hwmon0", &[("name", "r8169_0_500:00"), ("temp1_input", "48000"), ("temp1_max", "120000")])
    .link("/sys/class/hwmon/hwmon0/device", &format!("{pci}/0000:05:00.0/mdio_bus/r8169-0-500/r8169-0-500:00"))
    .dir(&format!("{pci}/0000:05:00.0/net/eno1"), &[])
    .dir("/sys/class/hwmon/hwmon1", &[("name", "r8169_0_700:00"), ("temp1_input", "35000"), ("temp1_max", "120000")])
    .link("/sys/class/hwmon/hwmon1/device", &format!("{pci}/0000:07:00.0/mdio_bus/r8169-0-700/r8169-0-700:00"))
    .dir(&format!("{pci}/0000:07:00.0/net/eno2"), &[])
    .dir(
        "/sys/class/hwmon/hwmon2",
        &[
            ("name", "nvme"),
            ("temp1_input", "42850"),
            ("temp1_label", "Composite"),
            ("temp1_max", "89850"),
            ("temp1_crit", "94850"),
            ("temp1_min", "-273150"),
            ("temp2_input", "41850"),
            ("temp2_label", "Sensor 1"),
            ("temp2_max", "65261850"),
            ("temp3_input", "35850"),
            ("temp3_label", "Sensor 2"),
        ],
    )
    .link("/sys/class/hwmon/hwmon2/device", &format!("{pci}/0000:01:00.0/nvme/nvme0"))
    .dir(&format!("{pci}/0000:01:00.0/nvme/nvme0"), &[("model", "KLEVV CRAS C925G M.2 NVMe SSD 2TB   ")])
    .dir(
        "/sys/class/hwmon/hwmon3",
        &[
            ("name", "k10temp"),
            ("temp1_input", "49500"),
            ("temp1_label", "Tctl"),
            ("temp3_input", "35375"),
            ("temp3_label", "Tccd1"),
            ("temp4_input", "35625"),
            ("temp4_label", "Tccd2"),
        ],
    )
    .link("/sys/class/hwmon/hwmon3/device", &format!("{pci}/0000:00:18.3"));
    for (k, a) in [(4, "1-0051"), (5, "1-0053")] {
        let d = format!("/sys/class/hwmon/hwmon{k}");
        fx.dir(
            &d,
            &[("name", "spd5118"), ("temp1_input", "33250"), ("temp1_max", "55000"), ("temp1_crit", "85000"), ("temp1_min", "0")],
        )
        .link(&format!("{d}/device"), &format!("{pci}/0000:00:14.0/i2c-1/{a}"));
    }
    fx.dir("/sys/class/hwmon/hwmon6", &[("name", "asus")])
        .link("/sys/class/hwmon/hwmon6/device", "/sys/devices/platform/eeepc-wmi")
        .dir(
            "/sys/class/hwmon/hwmon7",
            &[
                ("name", "amdgpu"),
                ("temp1_input", "43000"),
                ("temp1_label", "edge"),
                ("in0_input", "1030"),
                ("in0_label", "vddgfx"),
                ("in1_input", "1235"),
                ("in1_label", "vddnb"),
                ("power1_input", "10000"),
                ("power1_label", "PPT"),
            ],
        )
        .link("/sys/class/hwmon/hwmon7/device", &format!("{pci}/0000:0b:00.0"));
    // the motherboard chip: 7 fan headers (CPU_FAN and CPU_OPT in use), SmartFan IV
    let mut f: Vec<(String, String)> = vec![("name".into(), "nct6799".into())];
    for (n, rpm) in [(1, 0), (2, 640), (3, 0), (4, 0), (5, 621), (6, 0), (7, 0)] {
        f.push((format!("fan{n}_input"), rpm.to_string()));
        if n != 5 {
            f.push((format!("pwm{n}"), if n == 2 { "107" } else { "168" }.into()));
            f.push((format!("pwm{n}_enable"), "5".into()));
            f.push((format!("pwm{n}_mode"), "1".into()));
        }
    }
    let temps = [
        ("SYSTIN", 30000, Some(125000)),
        ("CPUTIN", 35000, Some(125000)),
        ("AUXTIN0", 35000, Some(125000)),
        ("AUXTIN1", 6000, Some(125000)),
        ("AUXTIN2", 19000, Some(127000)),
        ("AUXTIN3", -60000, Some(125000)),
        ("AUXTIN4", 85000, Some(100000)),
        ("PECI/TSI Agent 0 Calibration", 38000, None),
        ("AUXTIN5", 12000, None),
        ("PCH_CHIP_CPU_MAX_TEMP", 0, None),
        ("PCH_CHIP_TEMP", 0, None),
        ("PCH_CPU_TEMP", 0, None),
        ("TSI0_TEMP", 49625, None),
    ];
    for (k, (l, v, c)) in temps.iter().enumerate() {
        let n = k + 1;
        f.push((format!("temp{n}_input"), v.to_string()));
        f.push((format!("temp{n}_label"), l.to_string()));
        if let Some(c) = c {
            f.push((format!("temp{n}_crit"), c.to_string()));
            f.push((format!("temp{n}_max"), "80000".into()));
        }
    }
    let ins = [1032, 1000, 3392, 3328, 1008, 200, 840, 3392, 3280, 3328, 1376, 1120, 1056, 1000, 2032, 928, 1824, 1272];
    for (n, v) in ins.iter().enumerate() {
        f.push((format!("in{n}_input"), v.to_string()));
        f.push((format!("in{n}_min"), "0".into()));
        f.push((format!("in{n}_max"), if n == 0 { "1744" } else { "0" }.into()));
    }
    let fr: Vec<(&str, &str)> = f.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    fx.dir("/sys/class/hwmon/hwmon8", &fr)
        .link("/sys/class/hwmon/hwmon8/device", "/sys/devices/platform/nct6775.656");
    // RAPL: present, root-only
    fx.dir(
        "/sys/class/powercap/intel-rapl:0",
        &[("name", "package-0"), ("max_energy_range_uj", "65532610987"), ("energy_uj", "1")],
    )
    .dir(
        "/sys/class/powercap/intel-rapl:0:0",
        &[("name", "core"), ("max_energy_range_uj", "65532610987"), ("energy_uj", "1")],
    );
    for z in ["intel-rapl:0", "intel-rapl:0:0"] {
        let p = format!("{}/sys/class/powercap/{z}/energy_uj", fx.0);
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    }
    fx.dir(
        "/sys/bus/event_source/devices/power",
        &[("type", "14"), ("cpumask", "0")],
    )
    .dir(
        "/sys/bus/event_source/devices/power/events",
        &[("energy-pkg", "event=0x02"), ("energy-pkg.scale", "2.3283064365386962890625e-10")],
    )
    .dir("/sys/bus/event_source/devices/power_core", &[("type", "15"), ("cpumask", "0-15")])
    .dir(
        "/sys/bus/event_source/devices/power_core/events",
        &[("energy-core", "event=0x01"), ("energy-core.scale", "2.3283064365386962890625e-10")],
    );
    fx
}

#[test]
fn asus_b850_desktop() {
    let fx = asus_b850();
    let s = fx.sens();
    assert_eq!(s.board, "ASUS ProArt B850-CREATOR WIFI NEO");
    assert!(!s.vm);
    // fans: CPU_FAN and CPU_OPT spin, the other five headers are empty
    let fans: Vec<&Rd> = s.rd.iter().filter(|r| r.kind == K_FAN).collect();
    assert_eq!(fans.len(), 7);
    assert_eq!(fans.iter().filter(|r| r.seen).count(), 2);
    let cpu = fans.iter().find(|r| r.raw.contains("fan2")).unwrap();
    assert_eq!((cpu.name.as_str(), cpu.val), ("CPU fan", 640.0));
    assert!(cpu.raw.starts_with("CPU_FAN") && !cpu.guess);
    assert!((cpu.pwm - 107.0 / 255.0).abs() < 1e-3 && cpu.en == 5);
    let opt = fans.iter().find(|r| r.raw.contains("fan5")).unwrap();
    assert_eq!(opt.name, "CPU fan 2");
    assert!(opt.pwm < 0.0, "CPU_OPT has no pwm of its own");
    assert!(s.fan_hint.is_none());
    // temperatures: plain names, groups, bogus ones dropped
    assert_eq!(find(&s, "Tctl").name, "CPU");
    assert_eq!(find(&s, "Tccd2").name, "CPU chiplet 2");
    assert_eq!(find(&s, "SYSTIN").name, "motherboard");
    assert_eq!(find(&s, "SYSTIN").group, G_BOARD);
    assert_eq!(find(&s, "CPUTIN").group, G_BOARD);
    assert_eq!(find(&s, "TSI0_TEMP").group, G_CPU);
    assert!(find(&s, "TSI0_TEMP").minor);
    for bogus in ["AUXTIN1", "AUXTIN2", "AUXTIN3", "AUXTIN5", "PCH_CHIP_TEMP", "PCH_CPU_TEMP"] {
        assert!(!find(&s, bogus).ok, "{bogus} should be dropped");
    }
    assert!(find(&s, "AUXTIN0").ok && find(&s, "AUXTIN0").guess);
    assert!(find(&s, "AUXTIN4").ok, "85 °C is not provably bogus");
    assert!(!s.rd.iter().any(|r| r.raw.contains("Calibration")));
    let sticks: Vec<&str> = s.rd.iter().filter(|r| r.group == G_MEM).map(|r| r.name.as_str()).collect();
    assert_eq!(sticks, ["RAM stick 1", "RAM stick 2"]);
    let ssd = find(&s, "Composite");
    assert_eq!((ssd.name.as_str(), ssd.group, ssd.crit), ("SSD", G_DISK, 94.85));
    // (Sensor 1's 65261 °C limit is nonsense: the default applies)
    assert_eq!(find(&s, "Sensor 1").crit, 80.0);
    let nets: Vec<&str> = s.rd.iter().filter(|r| r.group == G_NET).map(|r| r.name.as_str()).collect();
    assert_eq!(nets, ["network eno1", "network eno2"]);
    assert_eq!(find(&s, "edge").name, "GPU");
    // voltages: Vcore, and the +5 V / +12 V rails through their dividers
    assert_eq!(find(&s, "in0").name, "CPU core (Vcore)");
    let p5 = find(&s, "in1");
    assert_eq!(p5.name, "+5 V");
    assert!((p5.val - 5.0).abs() < 0.01);
    assert!((find(&s, "in4").val - 12.096).abs() < 0.01);
    assert_eq!(find(&s, "in8").name, "CMOS battery");
    assert_eq!(find(&s, "vddnb").name, "SoC (northbridge)");
    // power: RAPL exists but is root-only (as for a normal user)
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(s.cpu_power, P_LOCKED);
        assert!(s.rapl_locked);
    }
    assert_eq!(s.role(R_GPU).map(|i| s.rd[i].name.as_str()), Some("GPU (built-in)"));
    assert!((s.rd[s.role(R_GPU).unwrap()].val - 0.01).abs() < 1e-6);
    assert!(s.has_core_pmu());
    // the page at the user's sizes, and small ones
    let t = page(fx.sens(), 128, 45);
    for want in [
        "sensors & power",
        "fans 2 spinning",
        "CPU fan ",
        "CPU_FAN · fan2",
        "board sets 42%",
        "5 headers with nothing connected",
        "temperatures all normal",
        "motherboard",
        "RAM stick 1",
        "CPU power needs permission",
        "cpuview --setup-sensors",
        "nct6799",
        "5 sens",
    ] {
        assert!(t.contains(want), "missing {want:?}\n{t}");
    }
    for (w, h) in [(415, 105), (275, 70), (172, 40), (80, 24), (60, 20), (40, 12), (20, 6)] {
        let t = page(fx.sens(), w, h);
        if w >= 80 {
            assert!(t.contains("CPU fan"), "{w}x{h}\n{t}");
        }
        if w >= 40 {
            assert!(t.contains(" 5 "), "tab 5 at {w}x{h}\n{t}");
        }
    }
}

/// An Intel ThinkPad: coretemp, a fan, RAPL readable, a battery on its own.
fn laptop() -> Fx {
    let fx = Fx::new("laptop");
    fx.dir(
        "/sys/class/dmi/id",
        &[("sys_vendor", "LENOVO"), ("product_name", "21AH0000US"), ("product_version", "ThinkPad T14 Gen 3")],
    )
    .dir("/proc", &[("cpuinfo", "processor\t: 0\nvendor_id\t: GenuineIntel")])
    .dir("/sys/class/hwmon/hwmon0", &[("name", "acpitz"), ("temp1_input", "45000"), ("temp1_crit", "128000")])
    .dir(
        "/sys/class/hwmon/hwmon1",
        &[
            ("name", "coretemp"),
            ("temp1_input", "55000"),
            ("temp1_label", "Package id 0"),
            ("temp1_max", "80000"),
            ("temp1_crit", "100000"),
            ("temp2_input", "51000"),
            ("temp2_label", "Core 0"),
            ("temp3_input", "53000"),
            ("temp3_label", "Core 1"),
            ("temp4_input", "50000"),
            ("temp4_label", "Core 2"),
            ("temp5_input", "57000"),
            ("temp5_label", "Core 3"),
        ],
    )
    .dir("/sys/class/hwmon/hwmon2", &[("name", "thinkpad"), ("fan1_input", "2100"), ("temp1_input", "47000")])
    .dir("/sys/class/hwmon/hwmon3", &[("name", "iwlwifi_1"), ("temp1_input", "40000")])
    .dir("/sys/class/thermal/thermal_zone0", &[("type", "x86_pkg_temp"), ("temp", "55000")])
    .dir("/sys/class/thermal/thermal_zone1", &[("type", "acpitz"), ("temp", "45000")])
    .dir(
        "/sys/class/powercap/intel-rapl:0",
        &[("name", "package-0"), ("max_energy_range_uj", "262143328850"), ("energy_uj", "262143000000")],
    )
    .dir("/sys/class/powercap/intel-rapl:0:0", &[("name", "core"), ("max_energy_range_uj", "262143328850"), ("energy_uj", "1000000")])
    .dir("/sys/class/powercap/intel-rapl:0:1", &[("name", "uncore"), ("max_energy_range_uj", "262143328850"), ("energy_uj", "1000000")])
    .dir("/sys/class/powercap/intel-rapl:1", &[("name", "psys"), ("max_energy_range_uj", "262143328850"), ("energy_uj", "1000000")])
    .dir(
        "/sys/class/power_supply/BAT0",
        &[
            ("type", "Battery"),
            ("capacity", "76"),
            ("status", "Discharging"),
            ("power_now", "12300000"),
            ("voltage_now", "16800000"),
            ("energy_now", "41000000"),
            ("energy_full", "50000000"),
        ],
    )
    .dir("/sys/class/power_supply/AC", &[("type", "Mains"), ("online", "0")])
    .dir("/sys/class/power_supply/hid-mouse-battery", &[("type", "Battery"), ("scope", "Device"), ("capacity", "40")]);
    fx
}

#[test]
fn intel_laptop_with_battery() {
    let fx = laptop();
    let mut s = fx.sens();
    assert_eq!(find(&s, "Package id 0").name, "CPU");
    assert_eq!(find(&s, "Package id 0").crit, 100.0);
    assert!(find(&s, "Core 3").minor && find(&s, "Core 3").name == "core 3");
    assert_eq!(find(&s, "temp1").group, G_OTHER); // acpitz
    assert!(!s.rd.iter().any(|r| r.raw == "x86_pkg_temp" || r.raw == "acpitz"), "zones duplicating hwmon");
    let fan = s.rd.iter().find(|r| r.kind == K_FAN).unwrap();
    assert_eq!((fan.name.as_str(), fan.val), ("fan", 2100.0));
    assert!(s.rd.iter().any(|r| r.name == "Wi-Fi" && r.group == G_NET));
    assert_eq!(s.cpu_power, P_OK);
    for role in [R_PKG, R_CORES, R_UNCORE, R_PSYS] {
        assert!(s.role(role).is_some(), "role {role}");
    }
    assert_eq!(s.bats.len(), 1, "the mouse's battery isn't the laptop's");
    let b = &s.bats[0];
    assert_eq!((b.pct, b.st), (76.0, 2));
    assert!((b.w - 12.3).abs() < 1e-3);
    assert!((b.left - 41.0 / 12.3 * 3600.0).abs() < 1.0);
    assert_eq!(s.ac_on, 0);
    // two seconds later the package counter has wrapped: 25 W, not a negative number
    let p = format!("{}/sys/class/powercap/intel-rapl:0/energy_uj", fx.0);
    // (328 850 µJ to the top, 49 671 150 after the wrap: 50 J in 2 s)
    std::fs::write(&p, "49671150").unwrap();
    let i = (0..s.srcs.len()).find(|&i| s.srcs[i].chip == "RAPL").unwrap();
    s.read_src(i, 3_000_000_000);
    let pk = &s.rd[s.role(R_PKG).unwrap()];
    assert!(pk.ok && (pk.val - 25.0).abs() < 0.01, "{}", pk.val);
    let t = page(fx.sens(), 128, 45);
    for want in ["CPU package", "battery", "76% · 3 h 20 min left", "12.3 W", "core 3", "fans 1 spinning"] {
        assert!(t.contains(want), "missing {want:?}\n{t}");
    }
}

/// A Raspberry Pi: one thermal zone, nothing else.
fn rpi() -> Fx {
    let fx = Fx::new("rpi");
    fx.dir("/proc/device-tree", &[("model", "Raspberry Pi 4 Model B Rev 1.4\0")])
        .dir("/proc", &[("cpuinfo", "processor\t: 0\nmodel name\t: ARMv7 Processor rev 3 (v7l)")])
        .dir("/sys/class/thermal/thermal_zone0", &[("type", "cpu-thermal"), ("temp", "48312")]);
    fx
}

#[test]
fn raspberry_pi_thermal_zone_only() {
    let fx = rpi();
    let s = fx.sens();
    assert_eq!(s.board, "Raspberry Pi 4 Model B Rev 1.4");
    let t: Vec<&Rd> = s.rd.iter().filter(|r| r.kind == K_TEMP).collect();
    assert_eq!(t.len(), 1);
    assert_eq!((t[0].name.as_str(), t[0].group, t[0].ok), ("CPU", G_CPU, true));
    assert!((t[0].val - 48.312).abs() < 1e-3);
    assert_eq!(s.cpu_power, P_NONE);
    assert!(s.fan_hint.is_none(), "no DMI: no PC driver to suggest");
    let t = page(fx.sens(), 80, 24);
    assert!(t.contains("temperatures all normal") && t.contains("48°C"), "{t}");
    assert!(t.contains("no power meters"), "{t}");
}

/// A virtual machine: no sensors at all.
fn vm() -> Fx {
    let fx = Fx::new("vm");
    fx.dir(
        "/sys/class/dmi/id",
        &[("sys_vendor", "QEMU"), ("product_name", "Standard PC (Q35 + ICH9, 2009)"), ("board_vendor", "")],
    )
    .dir("/sys/class/hwmon", &[]);
    fx
}

#[test]
fn virtual_machine_without_sensors() {
    let fx = vm();
    let s = fx.sens();
    assert!(s.vm);
    assert!(s.rd.is_empty() && s.fan_hint.is_none());
    assert_eq!(s.cpu_power, P_NONE);
    for (w, h) in [(128, 45), (80, 24), (40, 12)] {
        let t = page(fx.sens(), w, h);
        assert!(t.contains("virtual machine"), "{w}x{h}\n{t}");
    }
}

#[test]
fn missing_fan_driver_is_named() {
    let fx = Fx::new("nofan");
    fx.dir("/sys/class/dmi/id", &[("board_vendor", "Gigabyte Technology Co., Ltd."), ("board_name", "B650 AORUS ELITE AX")])
        .dir("/sys/class/hwmon/hwmon0", &[("name", "k10temp"), ("temp1_input", "41000"), ("temp1_label", "Tctl")])
        .dir(&format!("/lib/modules/{}/kernel/drivers/hwmon", release()), &[("it87.ko.zst", "")]);
    let s = fx.sens();
    assert_eq!(s.fan_hint, Some(("it87".to_string(), 0)));
    let t = page(fx.sens(), 128, 45);
    assert!(t.contains("the it87 driver isn't loaded"), "{t}");
}

#[test]
fn bmc_fans_are_not_a_missing_driver() {
    // a server board: the BMC runs the fans, so no Super-I/O driver to suggest
    let fx = Fx::new("bmcfan");
    fx.dir("/sys/class/dmi/id", &[("board_vendor", "Supermicro"), ("board_name", "X12SCZ-TLN4F")])
        .dir("/sys/class/hwmon/hwmon0", &[("name", "coretemp"), ("temp1_input", "41000"), ("temp1_label", "Package id 0")])
        .dir("/sys/class/ipmi/ipmi0", &[("dev", "243:0")])
        // the ACPI zone sits under /sys/devices/virtual, next to every virtual interface
        .dir("/sys/class/hwmon/hwmon1", &[("name", "acpitz"), ("temp1_input", "27800")])
        .link("/sys/class/hwmon/hwmon1/device", "/sys/devices/virtual/thermal/thermal_zone0")
        .dir("/sys/devices/virtual/thermal/thermal_zone0", &[("type", "acpitz")])
        .dir("/sys/devices/virtual/net/br-bdf23f2534a4", &[])
        .dir(&format!("/lib/modules/{}/kernel/drivers/hwmon", release()), &[("nct6775.ko.zst", "")]);
    let s = fx.sens();
    assert_eq!(s.fan_hint, Some(("ipmi".to_string(), 3)));
    assert!(s.bmc_locked, "no /dev/ipmi0 this user may open");
    let z = s.srcs.iter().find(|x| x.chip == "acpitz").unwrap();
    assert!(z.what != "network chip" && !z.dev.contains("br-"), "{} {}", z.what, z.dev);
    let t = page(fx.sens(), 128, 45);
    assert!(t.contains("BMC runs the fans") && t.contains("--setup-sensors --bmc"), "{t}");
}

#[test]
fn bmc_setup_step_is_valid_shell() {
    // the step's commands, run against a scratch directory instead of /etc and /dev
    let fx = Fx::new("bmcstep");
    let d = format!("{}/x", fx.0);
    std::fs::create_dir_all(format!("{d}/etc/udev/rules.d")).unwrap();
    std::fs::create_dir_all(format!("{d}/dev")).unwrap();
    let sh = format!("# {}", bmc_step("alice"))
        .replace(BMC_RULE, &format!("{d}{BMC_RULE}"))
        .replace("/dev/ipmi", &format!("{d}/dev/ipmi"))
        .replace("udevadm control --reload", "true");
    let out = std::process::Command::new("sh").args(["-ec", &sh]).output().unwrap();
    assert!(out.status.success(), "{sh}\n{}", String::from_utf8_lossy(&out.stderr));
    let rule = std::fs::read_to_string(format!("{d}{BMC_RULE}")).unwrap();
    assert_eq!(
        rule.lines().last().unwrap(),
        r#"SUBSYSTEM=="ipmi", KERNEL=="ipmi[0-9]*", OWNER="alice", MODE="0600""#
    );
    assert!(rule.starts_with("# cpuview --setup-sensors --bmc: let alice read the BMC (IPMI)\n"));
}

#[test]
fn rapl_counter_wraparound() {
    let r = 65_532_610_987u64;
    assert_eq!(wrap_delta(100, 300, r), 200);
    assert_eq!(wrap_delta(r - 50, 70, r), 120);
    assert_eq!(wrap_delta(500, 400, 0), 0, "perf counts never wrap");
    let mut c = Ctr {
        range: r,
        scale: 1e-6,
        ..Default::default()
    };
    assert_eq!(c.update(r - 1_000_000, 0), None);
    // too soon after (a RAPL counter ticks ~1 ms: short windows are noisy): wait
    assert_eq!(c.update(r - 900_000, 100_000_000), None);
    // 1 s later, past the wrap: 1 J + 1.5 J in 1 s = 2.5 W
    let w = c.update(1_500_000, 1_000_000_000).unwrap();
    assert!((w - 2.5).abs() < 1e-4, "{w}");
    // perf: 2^-32 J units
    let mut p = Ctr {
        perf: true,
        scale: parse_sci("2.3283064365386962890625e-10").unwrap(),
        ..Default::default()
    };
    p.update(0, 0);
    let w = p.update(117u64 << 32, 1_000_000_000).unwrap();
    assert!((w - 117.0).abs() < 1e-3, "{w}");
    assert_eq!(perf_event_cfg("event=0x02"), Some(2));
    assert_eq!(perf_event_cfg("event=0x02,umask=0x1"), None);
}

#[test]
fn plain_names() {
    let t = |c: &str, l: &str, n: u32| temp_name(c, l, n);
    assert_eq!(t("k10temp", "Tctl", 1), ("CPU".into(), G_CPU, 0));
    assert_eq!(t("k10temp", "Tccd3", 5).0, "CPU chiplet 3");
    assert_eq!(t("coretemp", "Package id 1", 1).0, "CPU 1");
    assert_eq!(t("nct6798", "SYSTIN", 1), ("motherboard".into(), G_BOARD, F_REF));
    assert_eq!(t("nct6798", "AUXTIN3", 6), ("aux input 3".into(), G_BOARD, F_AUX));
    assert_eq!(t("nct6798", "PCH_CHIP_TEMP", 11).0, "chipset");
    assert_eq!(t("it8689", "", 2), ("input 2".into(), G_BOARD, F_AUX));
    assert_eq!(t("asus_ec_sensors", "VRM", 3).0, "CPU power stage (VRM)");
    assert_eq!(t("asus_ec_sensors", "T_Sensor", 4).1, G_OTHER);
    assert_eq!(t("dell_smm", "SODIMM", 2).1, G_MEM);
    assert_eq!(t("nvme", "Sensor 2", 3), ("SSD sensor 2".into(), G_DISK, F_MINOR));
    assert_eq!(t("amdgpu", "junction", 2).0, "GPU hotspot");
    assert_eq!(t("cpu_thermal", "", 1).0, "CPU");
    assert_eq!(t("nct6799", "PECI/TSI Agent 0 Calibration", 8).2, F_DROP);
    let f = |c: &str, l: &str, n: u32, a: bool| fan_name(c, l, n, a);
    assert_eq!(f("nct6799", "", 2, true), ("CPU fan".into(), "CPU_FAN · fan2".into(), false));
    assert_eq!(f("nct6799", "", 5, true).0, "CPU fan 2");
    assert_eq!(f("nct6799", "", 1, true), ("case fan 1".into(), "CHA_FAN1? · fan1".into(), true));
    assert_eq!(f("nct6798", "", 3, false), ("fan 3".into(), "fan3".into(), true));
    assert_eq!(f("dell_smm", "Processor Fan", 1, false).0, "CPU fan");
    assert_eq!(f("asus_ec_sensors", "CPU_Opt", 1, true).0, "CPU fan 2");
    assert_eq!(f("amdgpu", "", 1, false).0, "GPU fan");
    let v = |c: &str, l: &str, n: u32, x: f32| volt_name(c, l, n, x);
    assert_eq!(v("nct6799", "", 1, 1.0), ("+5 V".into(), 5.0));
    assert_eq!(v("nct6799", "", 1, 0.4).1, 1.0, "no plausible +5 V: left as in1");
    assert_eq!(v("nct6799", "", 4, 1.008), ("+12 V".into(), 12.0));
    assert_eq!(v("it8686", "", 4, 1.0).0, "in4");
    assert_eq!(v("amdgpu", "vddgfx", 0, 1.0).0, "GPU core");
    // limits: nonsense ignored, defaults by part
    assert_eq!(temp_limits(G_DISK, Some(65261850), None), (70.0, 80.0));
    assert_eq!(temp_limits(G_CPU, None, None), (85.0, 95.0));
    assert_eq!(temp_limits(G_MEM, Some(55000), Some(85000)), (70.0, 85.0));
    assert!(!temp_ok(-60.0, false, None) && !temp_ok(0.0, false, None) && !temp_ok(127.0, false, None));
    assert!(!temp_ok(19.0, true, Some(30.0)) && temp_ok(35.0, true, Some(30.0)));
}

// ── the overview (view 0) on these machines ───────────────────────────────────

/// A fake /proc with a few programs; `ticks` scales their CPU time (two scans with
/// different values give CPU rates).
fn fake_proc(dir: &std::path::Path, ticks: u64) {
    let progs: [(u32, &str, u64, u64); 6] = [
        (100, "postgres", 400, 300_000),
        (101, "postgres", 300, 200_000),
        (102, "postgres", 10, 100_000),
        (200, "firefox", 90, 500_000),
        (300, "Xorg", 20, 50_000),
        (400, "a-program-with-a-very-long-name-indeed", 1, 10_000),
    ];
    for (pid, name, cpu, rss) in progs {
        let d = dir.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        let t = cpu * ticks;
        std::fs::write(
            d.join("stat"),
            format!("{pid} ({name}) S 1 {pid} {pid} 0 -1 0 0 0 0 0 {t} 0 0 0 20 0 1 0 1000 9000000 {rss} 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0\n"),
        )
        .unwrap();
        std::fs::write(d.join("cmdline"), format!("/usr/bin/{name}\0")).unwrap();
    }
    std::fs::write(dir.join("loadavg"), "1.50 0.75 0.25 2/300 999\n").unwrap();
}

/// The overview at w × h for these sensors, memory and network (as text), with the
/// fake processes; `f` adjusts the app first.
fn overview(s: Sens, m: crate::mem::Mem, n: crate::net::Net, w: usize, h: usize, f: &dyn Fn(&mut App)) -> String {
    let mut sm = crate::sys::Sampler::new_for_test(&(0..32).collect::<Vec<_>>());
    sm.t.groups = (0..16).map(|c| vec![c, c + 16]).collect();
    sm.t.ccd = (0..16).map(|c| c / 8).collect();
    sm.t.ndom = 2;
    sm.t.rank = vec![-1; 16];
    sm.t.maxf = 5760.0;
    sm.t.has_freq = true;
    for i in 0..32 {
        sm.f[i] = 3000.0 + i as f32 * 50.0;
        sm.u[i] = (i * 37 % 100) as f32 / 100.0;
        sm.ur[i] = sm.u[i];
    }
    for _ in 0..200 {
        sm.sample_fake();
    }
    let mut a = App::new(sm, crate::theme::load("default").unwrap());
    a.z = s;
    a.m = m;
    a.n = n;
    a.page = crate::ui::PG_OVERVIEW;
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("cpuview-ovproc-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    fake_proc(&dir, 1);
    let mut d = crate::procs::Procs::new(dir.to_str().unwrap());
    d.scan(1_000_000_000, 0);
    fake_proc(&dir, 2);
    d.scan(3_000_000_000, 0);
    a.ov.d = Some(d);
    a.ov_retarget(false);
    f(&mut a);
    a.first_size(w, h);
    a.bars_snap();
    a.sens_retarget(false);
    a.net_retarget(false);
    a.mem_retarget(false);
    let mut scr = Screen::new(w, h);
    a.draw(&mut scr, true);
    a.draw(&mut scr, false);
    a.draw_bars(&mut scr);
    // a live redraw shows what a full one does
    let mut s2 = Screen::new(w, h);
    a.draw(&mut s2, true);
    for y in 0..h {
        let row = |s: &Screen| (0..w).map(|x| s.buf[y * w + x].ch).collect::<String>();
        assert_eq!(row(&scr), row(&s2), "{w}x{h} row {y} differs after a live redraw");
    }
    // panels inside the screen, below the title and the strip, not overlapping
    let l = a.ov.lay;
    if h >= 4 {
        assert_eq!((l.strip.y, l.strip.h, l.strip.w), (1, 1, w), "{w}x{h}");
    }
    let rs: Vec<Rect> = l.rects().into_iter().filter(|r| r.w > 0).collect();
    for (i, r) in rs.iter().enumerate() {
        assert!(r.x + r.w <= w && r.y >= 2 && r.y + r.h < h, "{w}x{h} {r:?}");
        if !l.lines {
            assert!(r.h >= 3 && r.w >= 30, "{w}x{h} panel too small {r:?}");
        }
        for q in &rs[i + 1..] {
            let apart = r.x + r.w <= q.x || q.x + q.w <= r.x || r.y + r.h <= q.y || q.y + q.h <= r.y;
            assert!(apart, "{w}x{h} overlap {r:?} {q:?}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if std::env::var_os("OVLAY").is_some() {
        eprintln!("{:?}", a.ov.lay);
    }
    let mut t = String::new();
    for y in 0..h {
        for x in 0..w {
            t.push(scr.buf[y * w + x].ch);
        }
        t.push('\n');
    }
    assert!(!t.contains("NaN") && !t.contains("inf"), "{w}x{h}\n{t}");
    t
}

use crate::ui::Rect;

const SIZES: [(usize, usize); 9] = [(128, 45), (80, 24), (60, 20), (40, 12), (205, 50), (275, 70), (415, 105), (172, 40), (100, 30)];

#[test]
fn overview_on_this_desktop() {
    use crate::mem::Mem;
    use crate::net::Net;
    let fx = asus_b850();
    for (w, h) in SIZES {
        let t = overview(fx.sens(), Mem::fake(1, 3, 12), Net::fake(5, 3), w, h, &|a| a.n.ctrs.iter_mut().for_each(|c| c.health = 2));
        // every page's answer, at every size
        for want in ["CPU", "RAM", "disk", "↓", "postgres", "0 ", "5 "] {
            assert!(t.contains(want), "missing {want:?} at {w}x{h}\n{t}");
        }
        // all 16 cores (32 threads) as bars: the number 15 under the last core where
        // there is room for numbers
        if w >= 80 && h >= 24 {
            for want in ["RAM 17% in use", "containers 5 running", "programs · busiest: postgres ×3", "sensors", "CPU fan", "all normal", if w >= 128 { " 15 " } else { " 14 " }] {
                assert!(t.contains(want), "missing {want:?} at {w}x{h}\n{t}");
            }
        }
        if w >= 128 && h >= 45 {
            for want in ["busiest", "biggest", "firefox", "swap", "CPU load over time", "now", "1 min ago", "fullest", "free", "2 ›", "click a panel"] {
                assert!(t.contains(want), "missing {want:?} at {w}x{h}\n{t}");
            }
        }
        // RAPL is root-only here: no watts, but how to get them
        assert!(!t.contains(" W ·"), "{w}x{h}\n{t}");
    }
    // the big screens: every core's load over time beside the history charts
    let t = overview(fx.sens(), Mem::fake(1, 3, 12), Net::fake(5, 3), 275, 70, &|_| {});
    assert!(t.contains("load per core"), "{t}");
}

#[test]
fn overview_on_other_machines() {
    use crate::mem::Mem;
    use crate::net::Net;
    // a laptop: battery, CPU watts readable, one fan, no containers, no swap
    let fx = laptop();
    for (w, h) in SIZES {
        let t = overview(fx.sens(), Mem::fake(1, 1, 6), Net::fake(0, 0), w, h, &|a| {
            a.m.mi.swap_total = 0;
            a.m.mi.swap_free = 0;
        });
        assert!(!t.contains("containers"), "{w}x{h}\n{t}");
        if w >= 80 && h >= 24 {
            assert!(t.contains("battery") && t.contains("fan"), "{w}x{h}\n{t}");
        }
        if w >= 128 && h >= 45 {
            assert!(t.contains("no swap set up"), "{w}x{h}\n{t}");
        }
    }
    // a Raspberry Pi: one temperature, no fans, no power meters, no disk statistics
    let fx = rpi();
    for (w, h) in SIZES {
        let t = overview(fx.sens(), Mem::fake(0, 1, 3), Net::fake(0, 0), w, h, &|a| a.s.temps.clear());
        assert!(!t.contains("RPM") && !t.contains(" W "), "{w}x{h}\n{t}");
        if w >= 80 && h >= 24 {
            assert!(t.contains("storage") && t.contains("hottest"), "{w}x{h}\n{t}");
        }
    }
    // a virtual machine: no sensors (no panel), no disks, no filesystems, no network
    let fx = vm();
    for (w, h) in SIZES {
        let t = overview(fx.sens(), Mem::fake(0, 0, 3), Net::empty(), w, h, &|a| a.s.temps.clear());
        assert!(!t.contains("sensors") && !t.contains("RPM"), "{w}x{h}\n{t}");
        assert!(a_layout_has_no_disk(w, h) || !t.contains("disk idle"), "{w}x{h}\n{t}");
    }
}

fn a_layout_has_no_disk(_w: usize, _h: usize) -> bool {
    true
}

#[test]
fn overview_alerts() {
    use crate::mem::Mem;
    use crate::net::Net;
    let fx = asus_b850();
    let g = 1u64 << 30;
    // a full filesystem, RAM nearly full and swapping, the CPU at its limit, a
    // container unhealthy, the network down
    let t = overview(fx.sens(), Mem::fake(1, 3, 12), Net::fake(5, 3), 128, 45, &|a| {
        a.m.fs[0].avail = 40 * g;
        a.m.fs[0].used = 960 * g;
        a.m.mi.avail = g;
        a.m.swin = 30e6;
        a.s.temps = Vec::new();
        a.n.ctrs[1].health = 3;
        for i in a.n.ifs.iter_mut() {
            i.up = false;
        }
    });
    for want in ["act now", "RAM nearly full", "swap active", "/ 96% full · 42.9 GB free", "container", "unhealthy", "network not connected"] {
        assert!(t.lines().nth(1).unwrap().contains(want) || t.contains(want), "missing {want:?}\n{t}");
    }
    // the hottest part at its limit
    let t = overview(fx.sens(), Mem::fake(1, 3, 12), Net::fake(5, 3), 128, 45, &|a| {
        let k = a.z.rd.iter().position(|r| r.raw == "SYSTIN").unwrap();
        a.z.rd[k].val = 96.0;
    });
    assert!(t.lines().nth(1).unwrap().contains("motherboard warm: 96 °C"), "{t}");
    // (the SSD and the RAM sticks come from the memory page's helper thread, not read here)
    let t = overview(fx.sens(), Mem::fake(1, 3, 12), Net::fake(5, 3), 128, 45, &|a| {
        let k = a.z.rd.iter().position(|r| r.raw == "Composite").unwrap();
        a.z.rd[k].val = 96.0;
    });
    assert!(!t.contains("SSD too hot"), "{t}");
    // nothing: all normal
    let t = overview(fx.sens(), Mem::fake(1, 3, 12), Net::fake(5, 3), 128, 45, &|a| a.n.ctrs.iter_mut().for_each(|c| c.health = 2));
    assert!(t.lines().nth(1).unwrap().contains("all normal"), "{t}");
}

#[test]
fn overview_layouts_at_every_size() {
    use crate::mem::Mem;
    use crate::net::Net;
    let fx = asus_b850();
    for w in (40..=415).step_by(25) {
        for h in (12..=105).step_by(9) {
            overview(fx.sens(), Mem::fake(1, 3, 12), Net::fake(5, 3), w, h, &|_| {});
        }
    }
}

/// `W=128 H=45 cargo test --release dump_overview -- --ignored --nocapture`
#[test]
#[ignore]
fn dump_overview() {
    let w: usize = std::env::var("W").ok().and_then(|v| v.parse().ok()).unwrap_or(128);
    let h: usize = std::env::var("H").ok().and_then(|v| v.parse().ok()).unwrap_or(45);
    let fx = asus_b850();
    eprintln!("{}", overview(fx.sens(), crate::mem::Mem::fake(1, 3, 12), crate::net::Net::fake(5, 3), w, h, &|_| {}));
    let fx = laptop();
    eprintln!("{}", overview(fx.sens(), crate::mem::Mem::fake(1, 1, 6), crate::net::Net::fake(0, 0), w, h, &|_| {}));
}

/// Live redraw cost of the overview: `cargo test --release bench_overview -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_overview() {
    let fx = asus_b850();
    let (w, h) = (128, 45);
    let mut a = None;
    let _ = overview(fx.sens(), crate::mem::Mem::fake(1, 3, 12), crate::net::Net::fake(5, 3), w, h, &|_| {});
    // (the same set-up as `overview`, kept)
    let mut sm = crate::sys::Sampler::new_for_test(&(0..32).collect::<Vec<_>>());
    sm.t.groups = (0..16).map(|c| vec![c, c + 16]).collect();
    sm.t.ccd = (0..16).map(|c| c / 8).collect();
    sm.t.ndom = 2;
    sm.t.maxf = 5760.0;
    sm.t.has_freq = true;
    for _ in 0..200 {
        sm.sample_fake();
    }
    let mut app = App::new(sm, crate::theme::load("default").unwrap());
    app.z = fx.sens();
    app.m = crate::mem::Mem::fake(1, 3, 12);
    app.n = crate::net::Net::fake(5, 3);
    app.page = crate::ui::PG_OVERVIEW;
    app.first_size(w, h);
    let mut scr = Screen::new(w, h);
    app.draw(&mut scr, true);
    a.get_or_insert(0);
    let n = 2000;
    let t0 = crate::term::cpu_ns();
    for k in 0..n {
        for i in 0..32 {
            app.s.u[i] = ((i * 37 + k * 13) % 100) as f32 / 100.0;
            app.s.ur[i] = app.s.u[i];
        }
        app.s.sample_fake();
        app.m.mi.avail -= 1 << 20;
        app.bars_snap();
        app.draw(&mut scr, false);
    }
    eprintln!("live redraw: {} µs", (crate::term::cpu_ns() - t0) / n as i64 / 1000);
}

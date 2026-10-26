//! Sensors on Windows (the Windows half of sensors.rs). Windows has no user-mode
//! interface for CPU temperatures, fans on the board or voltages, and cpuview loads
//! no kernel driver; what it reads:
//! - the CPU's power (package, cores, built-in GPU, RAM where the CPU counts them)
//!   from Windows' energy meter (PDH "Energy Meter": the RAPL counters, no driver),
//! - graphics cards' temperature and fan (Windows' GPU statistics, as Task Manager
//!   shows them, any vendor) and NVIDIA cards' watts (NVML, which comes with the
//!   driver),
//! - the battery and mains (GetSystemPowerStatus; watts from the battery's IOCTLs),
//! - the firmware's ACPI thermal zones (PDH "Thermal Zone Information": often a fixed
//!   or board value, so never called the CPU's),
//! - NVMe drives' temperatures (a storage query, ~0.2 ms of wall time: slow cadence).

use super::*;
use crate::win::{close, cputemp, fd2h, from_pwstr, from_wide, h2fd, reg_str, svc, wide};
use windows_sys::Wdk::Graphics::Direct3D::*;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Ioctl::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Power::*;
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Windows sources (beyond S_BAT): ACPI zones, a drive, the energy meter, a
/// graphics card, cpuview's sensor service
pub(super) const S_ACPI: u8 = 6;
pub(super) const S_DRIVE: u8 = 7;
pub(super) const S_EMI: u8 = 9;
pub(super) const S_GPU: u8 = 10;
pub(super) const S_SVC: u8 = 11;

/// NVML's power reading: device, milliwatts
type NvPower = unsafe extern "C" fn(usize, *mut u32) -> i32;

/// What the Windows sources keep: the thermal zones' query, the energy meter's
/// query (energy, time), the graphics cards (D3DKMT handle, NVML device or 0).
pub struct State {
    zq: PDH_HQUERY,
    zc: PDH_HCOUNTER,
    zarr: Vec<u64>,
    /// tenths of a kelvin ("High Precision Temperature") or kelvin
    ztenths: bool,
    eq: PDH_HQUERY,
    ec: [PDH_HCOUNTER; 2],
    earr: Vec<u64>,
    gpus: Vec<(u32, usize)>,
    nv_power: Option<NvPower>,
    svc_gone: Option<i64>,
}

impl State {
    pub fn new() -> State {
        State {
            zq: std::ptr::null_mut(),
            zc: std::ptr::null_mut(),
            zarr: Vec::new(),
            ztenths: false,
            eq: std::ptr::null_mut(),
            ec: [std::ptr::null_mut(); 2],
            earr: Vec::new(),
            gpus: Vec::new(),
            nv_power: None,
            svc_gone: None,
        }
    }
}

/// What needs cpuview's sensor service, in one dim line (sensui.rs: hint kind 4; 5:
/// the service runs, the board's chip isn't one it reads).
fn driver_hint(svc: bool) -> Option<(String, u8)> {
    Some((String::new(), if svc { 5 } else { 4 }))
}

fn ioctl<T>(h: HANDLE, code: u32, inp: &[u8], out: &mut T) -> bool {
    let mut got = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            code,
            if inp.is_empty() { std::ptr::null() } else { inp.as_ptr() as *const _ },
            inp.len() as u32,
            out as *mut T as *mut _,
            std::mem::size_of::<T>() as u32,
            &mut got,
            std::ptr::null_mut(),
        ) != 0
    }
}

/// A drive's temperature (°C), as mem.rs's helper thread reads it.
fn drive_temp(h: HANDLE) -> Option<f32> {
    let mut q = [0u8; 12];
    q[..4].copy_from_slice(&(StorageDeviceTemperatureProperty as u32).to_le_bytes());
    let mut d: STORAGE_TEMPERATURE_DATA_DESCRIPTOR = unsafe { std::mem::zeroed() };
    if !ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut d) || d.InfoCount == 0 {
        return None;
    }
    Some(d.TemperatureInfo[0].Temperature as f32)
}

impl Sens {
    /// Find everything, once, and read it.
    pub fn detect(&mut self, now: i64) {
        self.detected = true;
        let bios = "HARDWARE\\DESCRIPTION\\System\\BIOS";
        let r = |v: &str| reg_str(HKEY_LOCAL_MACHINE, bios, v).unwrap_or_default();
        let (bv, bn, sv, pn) = (r("BaseBoardManufacturer"), r("BaseBoardProduct"), r("SystemManufacturer"), r("SystemProductName"));
        let vendor = short_vendor(if bv.is_empty() { sv.clone() } else { bv });
        let model = if !bn.is_empty() && !bn.starts_with("To be filled") { &bn } else { &pn };
        self.board = if model.is_empty() { String::new() } else { [&vendor, " ", model].concat() };
        self.vm = is_vm(&[&sv, " ", &pn].concat().to_ascii_lowercase());
        self.zones();
        self.emi();
        self.gpus();
        self.drives();
        if let Some(v) = svc::readings() {
            self.svc(&v);
        }
        self.batteries();
        self.finish_names();
        self.cpu_power = if self.role(R_PKG).is_some() { P_OK } else { P_NONE };
        if !self.vm {
            self.fan_hint = driver_hint(self.srcs.iter().any(|s| s.kind == S_SVC));
        }
        for i in 0..self.srcs.len() {
            self.read_src(i, now);
        }
    }

    /// cpuview's sensor service started or stopped since detection: its readings are
    /// added when it first answers (its snapshot, looked for every 10 s:
    /// svc::readings) and dropped when it hasn't for 20 s (they keep their last
    /// values until then).
    pub(super) fn win_tick(&mut self, now: i64) {
        if self.vm {
            return;
        }
        if let Some(s) = self.srcs.iter().position(|s| s.kind == S_SVC) {
            if self.w.svc_gone.is_some_and(|t| now - t >= 20 * SEC) {
                self.w.svc_gone = None;
                self.drop_src(s);
                self.fan_hint = driver_hint(false);
            }
        } else if let Some(v) = svc::readings() {
            let (t0, hists) = (thread_ns(), self.rd.iter().any(|r| r.hist.is_some()));
            self.svc(&v);
            self.finish_names();
            self.hists(hists);
            self.fan_hint = driver_hint(true);
            self.spent += (thread_ns() - t0).max(0);
            self.read_src(self.srcs.len() - 1, now);
        }
    }

    /// Source `s` and its readings gone (the sensor service stopped): the indices
    /// after them move down.
    fn drop_src(&mut self, s: usize) {
        let gone: Vec<bool> = self.rd.iter().map(|r| r.src as usize == s).collect();
        for b in &mut self.bats {
            b.rd -= gone[..b.rd].iter().filter(|&&g| g).count();
        }
        let mut k = 0;
        self.rd.retain(|_| {
            k += 1;
            !gone[k - 1]
        });
        for r in &mut self.rd {
            if r.src as usize > s {
                r.src -= 1;
            }
        }
        self.srcs.remove(s);
        self.cpu_power = if self.role(R_PKG).is_some() { P_OK } else { P_NONE };
    }

    /// cpuview's sensor service (win/svc.rs): the CPU's temperatures through PawnIO
    /// (the package, Tctl and the CCDs; each core's as minor ones).
    fn svc(&mut self, v: &[cputemp::Reading]) {
        let s = self.src("cpuview-sensors", "PawnIO", "cpuview's sensor service (PawnIO)", "", S_SVC, C_CPU, G_CPU);
        for (k, x) in v.iter().enumerate() {
            let name = match x.label.as_str() {
                "Pkg" => "CPU package".to_string(),
                "Tctl" => "CPU (Tctl)".to_string(),
                l if l.starts_with("Tccd") => ["CCD ", &l[4..]].concat(),
                l => l.replacen("Core", "core", 1),
            };
            let mut r = rd(K_TEMP, s, G_CPU, k as c_int, &x.label, &name);
            (r.warn, r.crit) = temp_limits(G_CPU, None, None);
            r.minor = x.minor;
            self.add(s, r);
        }
    }

    /// The firmware's thermal zones (one PDH query for all of them).
    fn zones(&mut self) {
        let w = &mut self.w;
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut w.zq) != 0 {
                return;
            }
            for (path, tenths) in [("\\Thermal Zone Information(*)\\High Precision Temperature", true), ("\\Thermal Zone Information(*)\\Temperature", false)] {
                if PdhAddEnglishCounterW(w.zq, wide(path).as_ptr(), 0, &mut w.zc) == 0 {
                    w.ztenths = tenths;
                    break;
                }
                w.zc = std::ptr::null_mut();
            }
            if w.zc.is_null() || PdhCollectQueryData(w.zq) != 0 {
                PdhCloseQuery(w.zq);
                w.zq = std::ptr::null_mut();
                return;
            }
        }
        let mut names = Vec::new();
        self.zone_values(&mut |n, _| names.push(n.to_string()));
        if names.is_empty() {
            return;
        }
        let (g, what, class) = chip_info("acpitz");
        let s = self.src("acpitz", "ACPI", what, "", S_ACPI, class, g);
        for (k, n) in names.iter().enumerate() {
            // ("\_TZ.TZ00" → "TZ00")
            let raw = n.rsplit('.').next().unwrap_or(n);
            let (name, tg, fl) = temp_name("acpitz", "", k as u32 + 1);
            let mut r = rd(K_TEMP, s, tg, k as c_int, raw, if names.len() == 1 { "firmware (ACPI) zone" } else { &name });
            r.minor = fl & F_MINOR != 0 && names.len() > 1;
            (r.warn, r.crit) = temp_limits(tg, None, None);
            self.add(s, r);
        }
    }

    /// Each zone's instance name and °C (one collection).
    fn zone_values(&mut self, f: &mut dyn FnMut(&str, f32)) {
        let w = &mut self.w;
        if w.zq.is_null() || unsafe { PdhCollectQueryData(w.zq) } != 0 {
            return;
        }
        let (mut size, mut n) = ((w.zarr.len() * 8) as u32, 0u32);
        let mut r = unsafe { PdhGetFormattedCounterArrayW(w.zc, PDH_FMT_DOUBLE, &mut size, &mut n, w.zarr.as_mut_ptr() as *mut _) };
        if r == PDH_MORE_DATA {
            w.zarr = vec![0u64; (size as usize).div_ceil(8) + 16];
            size = (w.zarr.len() * 8) as u32;
            r = unsafe { PdhGetFormattedCounterArrayW(w.zc, PDH_FMT_DOUBLE, &mut size, &mut n, w.zarr.as_mut_ptr() as *mut _) };
        }
        if r != 0 {
            return;
        }
        let it = unsafe { std::slice::from_raw_parts(w.zarr.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, n as usize) };
        for x in it {
            let k = unsafe { x.FmtValue.Anonymous.doubleValue } as f32;
            let c = if w.ztenths { k / 10.0 } else { k } - 273.15;
            f(&unsafe { from_pwstr(x.szName) }, if x.FmtValue.CStatus == 0 { c } else { f32::NAN });
        }
    }

    /// Windows' energy meter: the CPU's RAPL counters as Windows reads them (Intel,
    /// and AMD where it has them), no driver needed. Watts are the energy (EMI's
    /// unit: picowatt-hours) over the counter's own clock (ms), as its "Power"
    /// counter (mW) has them; domains the CPU lacks read 0 since boot and are left
    /// out.
    fn emi(&mut self) {
        let w = &mut self.w;
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut w.eq) != 0 {
                return;
            }
            let ok = PdhAddEnglishCounterW(w.eq, wide("\\Energy Meter(*)\\Energy").as_ptr(), 0, &mut w.ec[0]) == 0
                && PdhAddEnglishCounterW(w.eq, wide("\\Energy Meter(*)\\Time").as_ptr(), 0, &mut w.ec[1]) == 0;
            if !ok || PdhCollectQueryData(w.eq) != 0 {
                PdhCloseQuery(w.eq);
                w.eq = std::ptr::null_mut();
                return;
            }
        }
        let mut found = Vec::new();
        self.emi_values(&mut |n, e, _| {
            if e > 0 && !n.eq_ignore_ascii_case("_Total") {
                found.push(n.to_string());
            }
        });
        if found.is_empty() {
            return;
        }
        let s = self.src("Energy Meter", "EMI", "Windows' energy meter (RAPL)", "", S_EMI, C_POWER, G_CPU);
        for (k, n) in found.iter().enumerate() {
            let l = n.to_ascii_lowercase();
            let (role, g, pn) = if l.ends_with("_pkg") {
                (R_PKG, G_CPU, "CPU package")
            } else if l.ends_with("_pp0") {
                (R_CORES, G_CPU, "cores")
            } else if l.ends_with("_pp1") {
                (R_UNCORE, G_GPU, "GPU (built-in)")
            } else if l.ends_with("_dram") {
                (R_DRAM, G_MEM, "memory (RAM)")
            } else if l.contains("psys") || l.contains("platform") {
                (R_PSYS, G_OTHER, "whole computer")
            } else {
                (R_OTHER, G_OTHER, n.as_str())
            };
            let mut r = rd(K_POWER, s, g, k as c_int, n, pn);
            r.role = role;
            // (1 pWh = 3.6 nJ)
            r.ctr = Some(Ctr { scale: 3.6e-9, ..Default::default() });
            self.add(s, r);
        }
    }

    /// Each energy meter's name, energy (pJ) and time (ms): one collection.
    fn emi_values(&mut self, f: &mut dyn FnMut(&str, u64, i64)) {
        let w = &mut self.w;
        if w.eq.is_null() || unsafe { PdhCollectQueryData(w.eq) } != 0 {
            return;
        }
        let mut v: [Vec<(String, i64)>; 2] = Default::default();
        // (a real buffer from the start: the raw array call writes to it even when
        // only asked for its size)
        if w.earr.is_empty() {
            w.earr = vec![0u64; 256];
        }
        for (k, out) in v.iter_mut().enumerate() {
            let (mut size, mut n) = ((w.earr.len() * 8) as u32, 0u32);
            let mut r = unsafe { PdhGetRawCounterArrayW(w.ec[k], &mut size, &mut n, w.earr.as_mut_ptr() as *mut _) };
            if r == PDH_MORE_DATA {
                w.earr = vec![0u64; (size as usize).div_ceil(8) + 16];
                size = (w.earr.len() * 8) as u32;
                r = unsafe { PdhGetRawCounterArrayW(w.ec[k], &mut size, &mut n, w.earr.as_mut_ptr() as *mut _) };
            }
            if r != 0 {
                return;
            }
            let it = unsafe { std::slice::from_raw_parts(w.earr.as_ptr() as *const PDH_RAW_COUNTER_ITEM_W, n as usize) };
            *out = it.iter().map(|x| (unsafe { from_pwstr(x.szName) }, x.RawValue.FirstValue)).collect();
        }
        for (n, e) in &v[0] {
            if let Some(t) = v[1].iter().find(|x| x.0 == *n) {
                f(n, *e as u64, t.1);
            }
        }
    }

    /// Graphics cards: temperature and fan from Windows' own GPU statistics (what
    /// Task Manager shows; any vendor's driver, WDDM 2.4 and later), with the card's
    /// own limits; watts from NVML on NVIDIA's (the card on the same PCI bus).
    fn gpus(&mut self) {
        let mut e = D3DKMT_ENUMADAPTERS2::default();
        if unsafe { D3DKMTEnumAdapters2(&mut e) } < 0 || e.NumAdapters == 0 {
            return;
        }
        let mut v = vec![unsafe { std::mem::zeroed::<D3DKMT_ADAPTERINFO>() }; e.NumAdapters as usize];
        e.pAdapters = v.as_mut_ptr();
        if unsafe { D3DKMTEnumAdapters2(&mut e) } < 0 {
            return;
        }
        let mut cards = Vec::new();
        for a in &v[..e.NumAdapters as usize] {
            let mut pd: D3DKMT_ADAPTER_PERFDATA = unsafe { std::mem::zeroed() };
            let mut caps = D3DKMT_ADAPTER_PERFDATACAPS::default();
            if kmt(a.hAdapter, KMTQAITYPE_ADAPTERPERFDATA, &mut pd) && kmt(a.hAdapter, KMTQAITYPE_ADAPTERPERFDATA_CAPS, &mut caps) && (pd.Temperature > 0 || caps.MaxFanRPM > 0) {
                let mut reg: D3DKMT_ADAPTERREGISTRYINFO = unsafe { std::mem::zeroed() };
                kmt(a.hAdapter, KMTQAITYPE_ADAPTERREGISTRYINFO, &mut reg);
                let mut at = D3DKMT_ADAPTERADDRESS { BusNumber: u32::MAX, DeviceNumber: 0, FunctionNumber: 0 };
                kmt(a.hAdapter, KMTQAITYPE_ADAPTERADDRESS, &mut at);
                cards.push((a.hAdapter, from_wide(&reg.AdapterString), at.BusNumber, caps));
            } else {
                unsafe { D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: a.hAdapter }) };
            }
        }
        let nv = if cards.iter().any(|c| c.1.starts_with("NVIDIA")) { nvml() } else { None };
        self.w.nv_power = nv.as_ref().map(|n| n.0);
        let one = cards.len() == 1;
        for (k, (h, name, bus, caps)) in cards.into_iter().enumerate() {
            let what = if name.is_empty() { "graphics card".to_string() } else { name.clone() };
            let s = self.src("gpu", "WDDM", &what, &["PCI bus ", &n2s(bus)].concat(), S_GPU, 2, G_GPU);
            let label = |w: &str| if one || name.is_empty() { w.to_string() } else { [w, " (", &name, ")"].concat() };
            let mut t = rd(K_TEMP, s, G_GPU, k as c_int, "Temperature", &label("GPU"));
            let lim = |d: u32| (d > 0).then_some(d as i64 * 100);
            (t.warn, t.crit) = temp_limits(G_GPU, lim(caps.TemperatureWarning), lim(caps.TemperatureMax));
            self.add(s, t);
            if caps.MaxFanRPM > 0 {
                self.add(s, rd(K_FAN, s, G_GPU, k as c_int, "fan", &label("GPU fan")));
            }
            let dev = nv.as_ref().and_then(|n| n.1.iter().find(|d| d.0 == bus)).map_or(0, |d| d.1);
            if dev != 0 {
                let mut p = rd(K_POWER, s, G_GPU, k as c_int, "NVML", &label("GPU"));
                p.role = R_GPU;
                self.add(s, p);
            }
            self.w.gpus.push((h, dev));
        }
    }

    /// NVMe drives that report a temperature: one source each (read slowly).
    fn drives(&mut self) {
        let (g, what, class) = chip_info("nvme");
        let mut miss = 0;
        for i in 0..64 {
            let p = wide(&["\\\\.\\PhysicalDrive", &n2s(i)].concat());
            let h = unsafe { CreateFileW(p.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
            if h == INVALID_HANDLE_VALUE {
                miss += 1;
                if miss > 8 {
                    break;
                }
                continue;
            }
            miss = 0;
            if drive_temp(h).is_none() {
                close(h);
                continue;
            }
            let dev = ["disk", &n2s(i)].concat();
            let s = self.src("nvme", "stornvme", what, &dev, S_DRIVE, class, g);
            let (name, tg, _) = temp_name("nvme", "Composite", 1);
            let mut r = rd(K_TEMP, s, tg, h2fd(h), "Composite", &name);
            (r.warn, r.crit) = temp_limits(tg, None, None);
            self.add(s, r);
        }
    }

    /// The battery (GetSystemPowerStatus) and, where its driver answers, its watts.
    fn batteries(&mut self) {
        let mut ps: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        if unsafe { GetSystemPowerStatus(&mut ps) } == 0 || ps.BatteryFlag & 128 != 0 || ps.BatteryFlag == 255 {
            return;
        }
        let src = self.src("battery", "battery", "battery & charger", "", S_BAT, C_BAT, G_OTHER);
        let mut r = rd(K_POWER, src, G_OTHER, -1, "BAT0", "battery");
        r.role = R_BAT;
        self.add(src, r);
        let (h, tag) = battery_device().unwrap_or((INVALID_HANDLE_VALUE, 0));
        self.bats.push(Bat {
            name: "BAT0".into(),
            fds: [h2fd(h), tag as c_int, -1, -1, -1, -1, -1],
            charge: false,
            pct: -1.0,
            w: 0.0,
            st: 0,
            left: 0.0,
            rd: self.rd.len() - 1,
        });
    }

    pub(super) fn read_bats(&mut self, _now: i64) {
        let mut ps: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        if unsafe { GetSystemPowerStatus(&mut ps) } == 0 {
            return;
        }
        self.ac_on = match ps.ACLineStatus {
            0 => 0,
            1 => 1,
            _ => -1,
        };
        for b in &mut self.bats {
            b.pct = if ps.BatteryLifePercent <= 100 { ps.BatteryLifePercent as f32 } else { -1.0 };
            b.st = if ps.BatteryFlag & 8 != 0 {
                1
            } else if ps.ACLineStatus == 0 {
                2
            } else if b.pct >= 99.0 {
                3
            } else {
                4
            };
            b.left = if b.st == 2 && ps.BatteryLifeTime != u32::MAX { ps.BatteryLifeTime as f32 } else { 0.0 };
            // the rate (mW, negative while discharging) from the battery's driver
            let mut ws: BATTERY_WAIT_STATUS = unsafe { std::mem::zeroed() };
            ws.BatteryTag = b.fds[1] as u32;
            let mut st: BATTERY_STATUS = unsafe { std::mem::zeroed() };
            let inp = unsafe { std::slice::from_raw_parts(&ws as *const _ as *const u8, std::mem::size_of_val(&ws)) };
            b.w = if b.fds[0] >= 0 && ioctl(fd2h(b.fds[0]), IOCTL_BATTERY_QUERY_STATUS, inp, &mut st) && st.Rate != i32::MIN {
                st.Rate.unsigned_abs() as f32 / 1000.0
            } else {
                0.0
            };
        }
        for k in 0..self.bats.len() {
            let (i, w) = (self.bats[k].rd, self.bats[k].w);
            self.rd[i].val = w;
            self.rd[i].ok = self.bats[k].pct >= 0.0;
        }
    }

    /// Read a Windows source (ACPI zones, a drive, the energy meter, a graphics card).
    pub(super) fn read_win(&mut self, i: usize) {
        match self.srcs[i].kind {
            S_ACPI => {
                let mut v = Vec::new();
                self.zone_values(&mut |_, c| v.push(c));
                for r in self.rd.iter_mut().filter(|r| r.src as usize == i) {
                    let c = v.get(r.fd as usize).copied().unwrap_or(f32::NAN);
                    r.ok = temp_ok(c, false, None);
                    r.val = if r.ok { c } else { 0.0 };
                }
            }
            S_DRIVE => {
                for r in self.rd.iter_mut().filter(|r| r.src as usize == i) {
                    let c = drive_temp(fd2h(r.fd));
                    r.ok = c.is_some_and(|c| temp_ok(c, false, None));
                    r.val = c.unwrap_or(0.0);
                }
            }
            S_EMI => {
                let mut v = Vec::new();
                self.emi_values(&mut |n, e, t| v.push((n.to_string(), e, t)));
                for r in self.rd.iter_mut().filter(|r| r.src as usize == i) {
                    let x = v.iter().find(|x| x.0 == r.raw);
                    // (the counter's own clock: ms → ns)
                    if let Some(w) = x.and_then(|x| r.ctr.as_mut()?.update(x.1, x.2 * 1_000_000)) {
                        (r.val, r.ok, r.seen) = (w, true, true);
                    } else if x.is_none() {
                        r.ok = false;
                    }
                }
            }
            S_GPU => {
                let Some(k) = self.rd.iter().find(|r| r.src as usize == i).map(|r| r.fd as usize) else { return };
                let Some(&(h, dev)) = self.w.gpus.get(k) else { return };
                let mut pd: D3DKMT_ADAPTER_PERFDATA = unsafe { std::mem::zeroed() };
                let ok = kmt(h, KMTQAITYPE_ADAPTERPERFDATA, &mut pd);
                let mut mw = 0u32;
                let watts = (dev != 0 && self.w.nv_power.is_some_and(|f| unsafe { f(dev, &mut mw) } == 0)).then(|| mw as f32 / 1000.0);
                for r in self.rd.iter_mut().filter(|r| r.src as usize == i) {
                    let v = match r.kind {
                        K_TEMP => ok.then(|| pd.Temperature as f32 / 10.0).filter(|&c| temp_ok(c, false, None)),
                        K_FAN => ok.then_some(pd.FanRPM as f32).filter(|v| (0.0..30000.0).contains(v)),
                        _ => watts,
                    };
                    r.ok = v.is_some();
                    r.val = v.unwrap_or(0.0);
                    if r.ok && (r.kind != K_FAN || r.val > 0.0) {
                        r.seen = true;
                        r.max = r.max.max(r.val);
                    }
                }
            }
            S_SVC => {
                // the service's last snapshot, a fresh one asked for (none: the last
                // values stay, and win_tick lets the service go after 20 s)
                let Some(v) = svc::readings() else {
                    self.w.svc_gone.get_or_insert(crate::term::mono());
                    return;
                };
                self.w.svc_gone = None;
                for r in self.rd.iter_mut().filter(|r| r.src as usize == i) {
                    let val = v.iter().find(|x| x.label == r.raw).map_or(f32::NAN, |x| x.val);
                    r.ok = temp_ok(val, false, None);
                    if r.ok {
                        (r.val, r.seen) = (val, true);
                    }
                }
            }
            _ => {}
        }
    }

    /// (no BMC on Windows here)
    pub(super) fn read_bmc(&mut self, _i: usize, _wait_ms: i32) {}
}

/// One D3DKMT query of a graphics adapter into `out`.
fn kmt<T>(h: u32, ty: KMTQUERYADAPTERINFOTYPE, out: &mut T) -> bool {
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: h,
        Type: ty,
        pPrivateDriverData: out as *mut T as *mut _,
        PrivateDriverDataSize: std::mem::size_of::<T>() as u32,
    };
    unsafe { D3DKMTQueryAdapterInfo(&mut q) >= 0 }
}

/// NVIDIA's management library (installed with its driver, loaded from System32
/// only): its power reading, and each card's PCI bus and device. Started once
/// (~20 ms), never shut down.
fn nvml() -> Option<(NvPower, Vec<(u32, usize)>)> {
    unsafe {
        let lib = LoadLibraryExW(wide("nvml.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if lib.is_null() {
            return None;
        }
        let f = |n: &str| GetProcAddress(lib, [n, "\0"].concat().as_ptr());
        let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(f("nvmlInit_v2")?);
        let count: unsafe extern "C" fn(*mut u32) -> i32 = std::mem::transmute(f("nvmlDeviceGetCount_v2")?);
        let by_index: unsafe extern "C" fn(u32, *mut usize) -> i32 = std::mem::transmute(f("nvmlDeviceGetHandleByIndex_v2")?);
        // (nvmlPciInfo_t: legacy bus id[16], domain, bus, device, ids, bus id[32])
        let pci: unsafe extern "C" fn(usize, *mut [u32; 17]) -> i32 = std::mem::transmute(f("nvmlDeviceGetPciInfo_v3")?);
        let power: NvPower = std::mem::transmute(f("nvmlDeviceGetPowerUsage")?);
        let mut n = 0u32;
        if init() != 0 || count(&mut n) != 0 {
            return None;
        }
        let mut devs = Vec::new();
        for i in 0..n {
            let (mut d, mut p) = (0usize, [0u32; 17]);
            if by_index(i, &mut d) == 0 && pci(d, &mut p) == 0 {
                devs.push((p[5], d));
            }
        }
        Some((power, devs))
    }
}

/// The first battery's device and tag (for its rate in watts).
fn battery_device() -> Option<(HANDLE, u32)> {
    unsafe {
        let set = SetupDiGetClassDevsW(&GUID_DEVCLASS_BATTERY, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == -1 {
            return None;
        }
        let mut did: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
        did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        let mut out = None;
        if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &GUID_DEVCLASS_BATTERY, 0, &mut did) != 0 {
            let mut b = [0u64; 128];
            let d = b.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*d).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &did, d, 1024, std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                let path = (*d).DevicePath.as_ptr();
                let h = CreateFileW(path, FILE_GENERIC_READ | FILE_GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
                if h != INVALID_HANDLE_VALUE {
                    let mut tag = 0u32;
                    let wait = 0u32.to_le_bytes();
                    if ioctl(h, IOCTL_BATTERY_QUERY_TAG, &wait, &mut tag) && tag != 0 {
                        out = Some((h, tag));
                    } else {
                        close(h);
                    }
                }
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        out
    }
}

/// `cpuview --setup-sensors` on Windows: the sensor service (see win/setup.rs).
pub fn setup(undo: bool, _per_core: bool, _bmc: bool, dry: bool) -> i32 {
    crate::win::setup::setup(undo, dry)
}

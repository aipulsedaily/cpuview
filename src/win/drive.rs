//! Drives, for the memory page (win/mem.rs) and the sensors page (win/sensors.rs):
//! opened without access rights (enough for these queries, no administrator
//! needed), their model and temperature.

use super::wide;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::Ioctl::*;
use windows_sys::Win32::System::Power::GetDevicePowerState;
use windows_sys::Win32::System::IO::DeviceIoControl;

/// SMART attributes with the drive's temperature in their raw value's low byte:
/// Temperature, Airflow Temperature
const SMART_TEMP: [u8; 2] = [194, 190];

pub fn ioctl<T>(h: HANDLE, code: u32, inp: &[u8], out: &mut T) -> bool {
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

/// \\.\PhysicalDriveN, without access rights.
pub fn open(i: usize) -> HANDLE {
    let p = wide(&["\\\\.\\PhysicalDrive", &i.to_string()].concat());
    unsafe { CreateFileW(p.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) }
}

/// A drive's temperature (°C) and whether it came from its SMART attributes: what
/// the storage driver reports (NVMe drives, some others), else the SMART table's
/// (SATA drives: attribute 194 or 190, as Linux's drivetemp and smartctl read
/// them). The SMART table is read only while the drive is spun up: one that's
/// asleep isn't woken for its temperature (None then).
pub fn temp(h: HANDLE) -> Option<(f32, bool)> {
    let mut q = [0u8; 12];
    q[..4].copy_from_slice(&(StorageDeviceTemperatureProperty as u32).to_le_bytes());
    let mut d: STORAGE_TEMPERATURE_DATA_DESCRIPTOR = unsafe { std::mem::zeroed() };
    if ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut d) && d.InfoCount > 0 {
        let t = d.TemperatureInfo[0].Temperature as f32;
        return (t > -100.0 && t < 200.0).then_some((t, false));
    }
    let mut on = 1;
    if unsafe { GetDevicePowerState(h, &mut on) } != 0 && on == 0 {
        return None;
    }
    let mut p: STORAGE_PREDICT_FAILURE = unsafe { std::mem::zeroed() };
    if !ioctl(h, IOCTL_STORAGE_PREDICT_FAILURE, &[], &mut p) {
        return None;
    }
    smart_temp(&p.VendorSpecific).map(|t| (t, true))
}

/// The temperature in a SMART attribute table (a version word, then 30 entries of
/// 12 bytes: id, flags, value, worst, raw value, reserved).
pub fn smart_temp(b: &[u8]) -> Option<f32> {
    let attr = |id: u8| (0..30).map(|k| 2 + 12 * k).filter(|&o| o + 12 <= b.len()).find(|&o| b[o] == id).map(|o| b[o + 5]);
    SMART_TEMP.iter().find_map(|&id| attr(id)).filter(|&t| (1..100).contains(&t)).map(|t| t as f32)
}

/// The drive's vendor and product as it names itself.
pub fn model(h: HANDLE) -> String {
    let mut out = [0u64; 128];
    let q = [0u8; 12]; // StorageDeviceProperty, PropertyStandardQuery
    if !ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out) {
        return String::new();
    }
    let b = unsafe { std::slice::from_raw_parts(out.as_ptr() as *const u8, 1024) };
    let d = unsafe { &*(out.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    let at = |o: u32| -> String {
        let o = o as usize;
        if o == 0 || o >= b.len() {
            return String::new();
        }
        let n = b[o..].iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf8_lossy(&b[o..o + n]).trim().to_string()
    };
    let (v, p) = (at(d.VendorIdOffset), at(d.ProductIdOffset));
    let m = if v.is_empty() || p.starts_with(&v) { p } else { v + " " + &p };
    m.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_table() {
        let mut b = [0u8; 512];
        // (attribute 9 first, then 194: 37 °C, its lowest and highest after it)
        b[2] = 9;
        b[14] = 194;
        b[19..25].copy_from_slice(&[37, 0, 18, 0, 63, 0]);
        assert_eq!(smart_temp(&b), Some(37.0));
        b[14] = 190;
        assert_eq!(smart_temp(&b), Some(37.0));
        b[14] = 5;
        assert_eq!(smart_temp(&b), None);
        b[14] = 194;
        b[19] = 0;
        assert_eq!(smart_temp(&b), None);
    }
}

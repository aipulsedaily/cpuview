//! PawnIO (https://pawnio.eu), the signed driver LibreHardwareMonitor and others read
//! hardware through: cpuview's sensor service (win/svc.rs) talks to it only through
//! its device's IO controls, as its licence's exception for independent programs
//! asks. A handle runs one module: a signed program the driver checks and runs
//! (IntelMSR, AMDFamily17: PawnIO.Modules, LGPL-2.1), whose functions take and
//! return 64-bit words. The device opens for SYSTEM and administrators only.

use super::*;
use crate::win::wide;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;

const DEVICE: &str = r"\\?\GLOBALROOT\Device\PawnIO";
/// CTL_CODE(41394, function, METHOD_BUFFERED, FILE_ANY_ACCESS)
const fn ctl(function: u32) -> u32 {
    41394 << 16 | function << 2
}
const IOCTL_LOAD_BINARY: u32 = ctl(0x821);
const IOCTL_EXECUTE_FN: u32 = ctl(0x841);
/// a function's name, NUL-padded, before its input words
const NAME_LEN: usize = 32;

/// Whether the driver is installed: its device exists (opening it is for SYSTEM and
/// administrators, so "access denied" means it's there).
pub fn installed() -> bool {
    match Module::open_device() {
        Ok(h) => {
            close(h);
            true
        }
        Err(e) => e == ERROR_ACCESS_DENIED,
    }
}

/// A module loaded into the driver (its own handle).
pub struct Module {
    h: HANDLE,
}

impl Module {
    fn open_device() -> Result<HANDLE, u32> {
        let h = unsafe {
            CreateFileW(
                wide(DEVICE).as_ptr(),
                0xC000_0000, // GENERIC_READ | GENERIC_WRITE
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(unsafe { GetLastError() })
        } else {
            Ok(h)
        }
    }

    /// Open the driver and load `blob` (a signed module) into it.
    pub fn load(blob: &[u8]) -> Option<Module> {
        let h = Self::open_device().ok()?;
        let m = Module { h };
        m.ioctl(IOCTL_LOAD_BINARY, blob, &mut []).map(|_| m)
    }

    /// Run the module's function `name` on `input`; the words it wrote into `out`.
    pub fn call(&self, name: &str, input: &[u64], out: &mut [u64]) -> Option<usize> {
        let buf = call_buf(name, input)?;
        let n = self.ioctl(IOCTL_EXECUTE_FN, &buf, unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr() as *mut u8, out.len() * 8) })?;
        Some(n / 8)
    }

    /// One word in, one out.
    pub fn read(&self, name: &str, arg: u64) -> Option<u64> {
        let mut out = [0u64];
        (self.call(name, &[arg], &mut out)? == 1).then_some(out[0])
    }

    fn ioctl(&self, code: u32, input: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                code,
                input.as_ptr() as *const _,
                input.len() as u32,
                if out.is_empty() { std::ptr::null_mut() } else { out.as_mut_ptr() as *mut _ },
                out.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(got as usize)
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        close(self.h);
    }
}

/// The execute call's input: the function's name in 32 bytes (NUL-padded, at most
/// 31 characters), then the input words.
fn call_buf(name: &str, input: &[u64]) -> Option<Vec<u8>> {
    if name.len() >= NAME_LEN {
        return None;
    }
    let mut b = vec![0u8; NAME_LEN + 8 * input.len()];
    b[..name.len()].copy_from_slice(name.as_bytes());
    for (k, w) in input.iter().enumerate() {
        b[NAME_LEN + 8 * k..NAME_LEN + 8 * k + 8].copy_from_slice(&w.to_le_bytes());
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_controls() {
        // (as PawnIO's pawnio_um.h defines them)
        assert_eq!(IOCTL_LOAD_BINARY, 0xA1B2_2084);
        assert_eq!(IOCTL_EXECUTE_FN, 0xA1B2_2104);
        let b = call_buf("ioctl_read_msr", &[0x19c]).unwrap();
        assert_eq!(b.len(), 40);
        assert_eq!(&b[..15], b"ioctl_read_msr\0");
        assert_eq!(&b[32..], &0x19cu64.to_le_bytes());
        assert!(call_buf(&"x".repeat(32), &[]).is_none());
    }

    #[test]
    fn not_installed_is_no_error() {
        // (the driver isn't on CI's runners: loading fails, nothing panics)
        if !installed() {
            assert!(Module::load(&[0u8; 16]).is_none());
        }
    }
}

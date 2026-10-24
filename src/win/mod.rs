//! Shared helpers for the Windows sources. Each of those is the Windows half of the
//! module it serves, declared there (`sys` → win/cpu.rs, `mem` → win/mem.rs …) so it
//! fills the same structures; `term` (the console) is loaded in place of term.rs.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{VirtualQuery, VirtualUnlock, MEMORY_BASIC_INFORMATION, MEM_IMAGE};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};

// (not in windows-sys: processor groups beyond the caller's)
windows_link::link!("ntdll.dll" "system" fn NtQuerySystemInformationEx(class: i32, input: *const core::ffi::c_void, inlen: u32, info: *mut core::ffi::c_void, len: u32, retlen: *mut u32) -> i32);

/// NUL-terminated UTF-16 for the W APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to the first NUL.
pub fn from_wide(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

/// A NUL-terminated UTF-16 string at `p`.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string.
pub unsafe fn from_pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// A registry string value (REG_SZ).
pub fn reg_str(root: HKEY, key: &str, val: &str) -> Option<String> {
    let mut b = [0u16; 256];
    let mut n = (b.len() * 2) as u32;
    let (k, v) = (wide(key), wide(val));
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), b.as_mut_ptr() as *mut _, &mut n)
    };
    (r == 0).then(|| from_wide(&b).trim().to_string())
}

/// A registry number (REG_DWORD).
pub fn reg_u32(root: HKEY, key: &str, val: &str) -> Option<u32> {
    let mut d = 0u32;
    let mut n = 4u32;
    let (k, v) = (wide(key), wide(val));
    let r = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut d as *mut u32 as *mut _, &mut n)
    };
    (r == 0).then_some(d)
}

pub fn fd2h(fd: i32) -> HANDLE {
    fd as isize as HANDLE
}

pub fn close(h: HANDLE) {
    if !h.is_null() && h != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(h) };
    }
}

/// Code pages out of the working set (see main.rs `trim_code`): every executable
/// image mapping (the program and its DLLs). VirtualUnlock on pages that aren't locked
/// removes them from the working set; they stay in memory and come back on use.
pub fn trim_code() {
    let mut a = 0usize;
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    while unsafe { VirtualQuery(a as *const _, &mut m, sz) } == sz {
        // PAGE_EXECUTE_READ (0x20) / PAGE_EXECUTE (0x10): code, not writable data
        if m.Type == MEM_IMAGE && matches!(m.Protect, 0x10 | 0x20) {
            unsafe { VirtualUnlock(m.BaseAddress, m.RegionSize) };
        }
        let next = (m.BaseAddress as usize).wrapping_add(m.RegionSize);
        if next <= a {
            break;
        }
        a = next;
    }
}

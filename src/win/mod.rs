//! Shared helpers for the Windows sources. Each of those is the Windows half of the
//! module it serves, declared there (`sys` → win/cpu.rs, `mem` → win/mem.rs …) so it
//! fills the same structures; `term` (the console) is loaded in place of term.rs.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{VirtualQuery, VirtualUnlock, MEMORY_BASIC_INFORMATION, MEM_IMAGE};

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

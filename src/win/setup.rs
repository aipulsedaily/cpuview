//! `cpuview --setup-sensors` on Windows: what cpuview reads and how to see more, and
//! installing / removing cpuview's sensor service (win/svc.rs): CPU temperatures
//! through the PawnIO driver, which it doesn't install (`winget install
//! namazso.PawnIO`).
//!
//! It shows what it would do and asks first; then it runs itself elevated (one UAC
//! prompt), and that does everything in places only administrators may write: a SYSTEM
//! service must never run a program from a folder the user can change. The modules
//! come from PawnIO.Modules' release, pinned by SHA-256 (the driver checks their
//! signatures too). Where things go never comes from the user's environment, which
//! the elevated run inherits (Program Files from the registry, System32 from Windows),
//! and its log is in the program's folder there: never anywhere a user may create
//! files or links (%ProgramData%'s default lets them), which an elevated program
//! could be made to overwrite any file through.

use super::svc::{self, SERVICE};
use super::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{GetLastError, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

/// PawnIO.Modules' signed release, and the modules taken from it (with its licence)
const MODULES_URL: &str = "https://github.com/namazso/PawnIO.Modules/releases/download/0.2.11/release_0_2_11.zip";
const MODULES_SHA256: &str = "43608cb89bc84247fef1368a139013f7d043e17db6d6c8dfc9b46bf0905a81f4";
const MODULES: [(&str, &str); 2] = [
    ("IntelMSR.bin", "d6ed85d65ab17a22f813ef98207d6d537155ee2ded5976a21cb48413c9b92e5f"),
    ("AMDFamily17.bin", "dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91"),
];

/// %ProgramFiles%\cpuview, Program Files as the registry has it (only administrators
/// may change that; the environment variable is the user's).
fn program_dir() -> PathBuf {
    let pf = reg_str(HKEY_LOCAL_MACHINE, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion", "ProgramFilesDir").unwrap_or_else(|| "C:\\Program Files".into());
    PathBuf::from(pf).join("cpuview")
}

/// A program of Windows' own (System32, as Windows says): never one found on the
/// PATH, which runs elevated through folders the user may write.
fn system32(exe: &str) -> PathBuf {
    let mut b = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(b.as_mut_ptr(), b.len() as u32) } as usize;
    let dir = if n > 0 && n < b.len() { String::from_utf16_lossy(&b[..n]) } else { "C:\\Windows\\System32".into() };
    PathBuf::from(dir).join(exe)
}

/// The elevated install's log, in the program's folder (only administrators may
/// write there).
fn log_path() -> PathBuf {
    program_dir().join("setup.log")
}

/// `cpuview --setup-sensors [--undo] [--dry-run]`.
pub fn setup(undo: bool, dry: bool) -> i32 {
    if std::env::args().any(|a| a == "--elevated") {
        return elevated(undo);
    }
    let svc_state = svc::installed();
    println!(
        "cpuview on Windows reads what Windows itself offers: CPU load and clocks, memory,
disks, network, processes, the CPU's power (Windows' energy meter), graphics cards'
temperature, fan and (NVIDIA) watts, the battery, NVMe drive temperatures and the
firmware's ACPI thermal zones. CPU temperatures, the board's fans (CPU fan, case
fans, pump) and voltages need a kernel driver.

CPU temperatures: cpuview's sensor service reads them through PawnIO, a signed
driver (https://pawnio.eu) for hardware tools. The service runs as
SYSTEM, only reads, and idles until cpuview asks; cpuview runs as you."
    );
    println!();
    println!("now: PawnIO driver {}", if pawnio::installed() { "installed" } else { "not installed" });
    println!(
        "     sensor service {}",
        match svc_state {
            Some(true) => "installed, running",
            Some(false) => "installed, not running",
            None => "not installed",
        }
    );
    println!();
    let dir = program_dir();
    if undo {
        if svc_state.is_none() && !dir.exists() {
            println!("Nothing to remove.");
            return 0;
        }
        println!("To remove: the {SERVICE} service and {}.", dir.display());
    } else {
        if !pawnio::installed() {
            println!("First install PawnIO (it asks for administrator rights):\n    winget install namazso.PawnIO\nthen run cpuview --setup-sensors again.");
            return 1;
        }
        println!("To install (asks for administrator rights once):");
        println!("  - copy cpuview.exe to {}", dir.display());
        println!("  - download PawnIO.Modules 0.2.11 (signed modules, LGPL-2.1) and keep");
        println!("    {} with its licence in {}", MODULES.map(|m| m.0).join(", "), dir.join("pawnio").display());
        println!("  - add the {SERVICE} service (starts with Windows, runs as SYSTEM, read-only)");
        println!("Undo with: cpuview --setup-sensors --undo");
    }
    if dry {
        return 0;
    }
    print!("\nGo ahead? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut a = String::new();
    let _ = std::io::stdin().read_line(&mut a);
    if !matches!(a.trim(), "y" | "Y" | "yes") {
        println!("Nothing changed.");
        return 1;
    }
    let code = run_elevated(if undo { "--setup-sensors --undo --elevated" } else { "--setup-sensors --elevated" });
    if !undo {
        if let Ok(log) = std::fs::read_to_string(log_path()) {
            print!("{log}");
        }
    }
    match code {
        Some(0) => {
            println!("{}", if undo { "Removed." } else { "Done: cpuview shows CPU temperatures now (on the CPU and Sensors & Power pages)." });
            0
        }
        Some(U_SERVICE) => {
            println!("The {SERVICE} service couldn't be removed.");
            U_SERVICE as i32
        }
        Some(U_FILES) => {
            println!("The service is gone, but {} couldn't be removed (in use?).", program_dir().display());
            U_FILES as i32
        }
        Some(c) => {
            println!("It didn't finish (code {c}).");
            c as i32
        }
        None => {
            println!("Administrator rights weren't given: nothing changed.");
            1
        }
    }
}

/// This program, elevated, with `args`; its exit code (None: not started).
fn run_elevated(args: &str) -> Option<u32> {
    let exe = std::env::current_exe().ok()?;
    let (verb, file, params) = (wide("runas"), wide(&exe.to_string_lossy()), wide(args));
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = params.as_ptr();
    sei.nShow = 0; // SW_HIDE
    if unsafe { ShellExecuteExW(&mut sei) } == 0 || sei.hProcess.is_null() {
        return None;
    }
    let mut code = 1u32;
    unsafe {
        if WaitForSingleObject(sei.hProcess, INFINITE) == WAIT_OBJECT_0 {
            GetExitCodeProcess(sei.hProcess, &mut code);
        }
    }
    close(sei.hProcess);
    Some(code)
}

/// The elevated part: install (its steps logged) or remove (what failed in its exit
/// code: the folder the log would be in goes).
fn elevated(undo: bool) -> i32 {
    if undo {
        return remove() as i32;
    }
    let _ = std::fs::create_dir_all(program_dir());
    let mut log = std::fs::File::create(log_path()).ok();
    let mut say = |s: &str| {
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{s}");
        }
    };
    match install(&mut say) {
        Ok(()) => 0,
        Err(e) => {
            say(&["failed: ", &e].concat());
            3
        }
    }
}

fn install(say: &mut dyn FnMut(&str)) -> Result<(), String> {
    let dir = program_dir();
    let mods = dir.join("pawnio");
    std::fs::create_dir_all(&mods).map_err(|e| format!("{}: {e}", mods.display()))?;
    // (a running service holds its program: stop it before replacing it)
    service(|s| {
        let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        unsafe { ControlService(s, SERVICE_CONTROL_STOP, &mut st) };
        wait_stopped(s);
    });
    let exe = dir.join("cpuview.exe");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    std::fs::copy(&me, &exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    say(&["copied cpuview.exe to ", &exe.to_string_lossy()].concat());
    // the modules: the release (checked), the two modules and the licence (checked)
    let zip = mods.join("release.zip");
    let ok = std::process::Command::new(system32("curl.exe"))
        .args(["-fsSL", "--proto", "=https", "--max-time", "60", "-o"])
        .arg(&zip)
        .arg(MODULES_URL)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return Err("downloading PawnIO.Modules failed".into());
    }
    let sum = sha256_file(&zip);
    if sum.as_deref() != Some(MODULES_SHA256) {
        let _ = std::fs::remove_file(&zip);
        return Err("PawnIO.Modules' download isn't the release cpuview knows (SHA-256)".into());
    }
    let mut tar = std::process::Command::new(system32("tar.exe"));
    tar.arg("-xf").arg(&zip).arg("-C").arg(&mods).arg("COPYING");
    for (m, _) in MODULES {
        tar.arg(m);
    }
    let ok = tar.status().is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&zip);
    if !ok {
        return Err("unpacking PawnIO.Modules failed".into());
    }
    for (m, h) in MODULES {
        if sha256_file(&mods.join(m)).as_deref() != Some(h) {
            return Err([m, ": not the module cpuview knows (SHA-256)"].concat());
        }
    }
    say(&["PawnIO.Modules 0.2.11: ", &MODULES.map(|m| m.0).join(", "), " in ", &mods.to_string_lossy()].concat());
    // the service: added (or its program path set again), then started
    let bin = ["\"", &exe.to_string_lossy(), "\" --sensor-service"].concat();
    unsafe {
        let m = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if m.is_null() {
            return Err(["service manager: error ", &GetLastError().to_string()].concat());
        }
        let mut s = OpenServiceW(m, wide(SERVICE).as_ptr(), SERVICE_ALL_ACCESS);
        if s.is_null() {
            s = CreateServiceW(
                m,
                wide(SERVICE).as_ptr(),
                wide("cpuview sensors").as_ptr(),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                wide(&bin).as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            );
        } else {
            ChangeServiceConfigW(s, SERVICE_NO_CHANGE, SERVICE_AUTO_START, SERVICE_NO_CHANGE, wide(&bin).as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null());
        }
        if s.is_null() {
            let e = GetLastError();
            CloseServiceHandle(m);
            return Err(["adding the service: error ", &e.to_string()].concat());
        }
        let mut desc = wide("CPU temperatures for cpuview, read-only, through the PawnIO driver. Idle unless cpuview asks.");
        let d = SERVICE_DESCRIPTIONW { lpDescription: desc.as_mut_ptr() };
        ChangeServiceConfig2W(s, SERVICE_CONFIG_DESCRIPTION, &d as *const _ as *const _);
        // (only the privileges it needs: making its global snapshot)
        let mut privs: Vec<u16> = "SeCreateGlobalPrivilege\0SeChangeNotifyPrivilege\0\0".encode_utf16().collect();
        let p = SERVICE_REQUIRED_PRIVILEGES_INFOW { pmszRequiredPrivileges: privs.as_mut_ptr() };
        ChangeServiceConfig2W(s, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO, &p as *const _ as *const _);
        let started = StartServiceW(s, 0, std::ptr::null()) != 0;
        let e = GetLastError();
        CloseServiceHandle(s);
        CloseServiceHandle(m);
        if !started {
            return Err(["starting the service: error ", &e.to_string()].concat());
        }
    }
    say(&["service ", SERVICE, ": added and started"].concat());
    Ok(())
}

/// `--undo`'s exit codes: the service, or the folder, couldn't be removed
const U_SERVICE: u32 = 4;
const U_FILES: u32 = 5;

/// Stop and delete the service, then its folder: 0, or what failed.
fn remove() -> u32 {
    let mut gone = true;
    service(|s| {
        let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        unsafe { ControlService(s, SERVICE_CONTROL_STOP, &mut st) };
        wait_stopped(s);
        gone = unsafe { DeleteService(s) } != 0;
    });
    if !gone {
        return U_SERVICE;
    }
    let dir = program_dir();
    if dir.exists() && std::fs::remove_dir_all(&dir).is_err() {
        return U_FILES;
    }
    0
}

/// Run `f` on the service, if it is installed.
fn service(f: impl FnOnce(SC_HANDLE)) {
    unsafe {
        let m = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if m.is_null() {
            return;
        }
        let s = OpenServiceW(m, wide(SERVICE).as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS | 0x10000); // DELETE
        if !s.is_null() {
            f(s);
            CloseServiceHandle(s);
        }
        CloseServiceHandle(m);
    }
}

/// Up to 10 s for the service to stop.
fn wait_stopped(s: SC_HANDLE) {
    for _ in 0..100 {
        let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        if unsafe { QueryServiceStatus(s, &mut st) } == 0 || st.dwCurrentState == SERVICE_STOPPED {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A file's SHA-256 in hex (Windows' own hash).
fn sha256_file(p: &Path) -> Option<String> {
    let b = std::fs::read(p).ok()?;
    let mut h = [0u8; 32];
    let st = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0, b.as_ptr(), b.len() as u32, h.as_mut_ptr(), 32) };
    (st >= 0).then(|| h.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where things go: from the registry and Windows, whatever the environment says.
    #[test]
    fn places() {
        let (pd, tar) = (program_dir(), system32("tar.exe"));
        assert!(pd.is_absolute() && pd.ends_with("cpuview"), "{}", pd.display());
        assert_eq!(log_path().parent(), Some(pd.as_path()));
        assert!(tar.is_absolute() && tar.exists(), "{}", tar.display());
    }

    #[test]
    fn hashes() {
        let p = std::env::temp_dir().join(["cpuview-sha-", &std::process::id().to_string()].concat());
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).as_deref(), Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
        let _ = std::fs::remove_file(&p);
    }
}

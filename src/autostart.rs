//! Start with Windows: one value, `DeskFrame Widget`, in the current user's
//! Run key, holding the quoted path of this .exe. Removing it is the whole
//! uninstall. Not plain `DeskFrame`: another app of that name (the one this
//! project replaces) uses that value, and toggling ours must never touch it.

use std::ffi::c_void;
use std::path::Path;

use windows::core::{w, Error, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, E_FAIL};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: &str = "DeskFrame Widget";

/// True when Windows will start this .exe at sign-in.
pub fn is_enabled() -> bool {
    is_set(RUN_KEY, VALUE)
}

/// Turns start-at-sign-in on (pointing at this .exe) or off.
pub fn set(enabled: bool) -> Result<()> {
    set_value(RUN_KEY, VALUE, enabled)
}

/// What the Run value must hold for `exe`: its path in quotes; `None`
/// without a path (an empty value would start nothing).
fn command(exe: Option<&Path>) -> Option<String> {
    exe.filter(|p| !p.as_os_str().is_empty())
        .map(|p| format!("\"{}\"", p.display()))
}

fn this_exe() -> Option<String> {
    command(std::env::current_exe().ok().as_deref())
}

/// True when the value exists and points at this .exe (a value left by a
/// copy elsewhere does not count).
fn is_set(key: PCWSTR, name: &str) -> bool {
    let Some(expected) = this_exe() else {
        return false;
    };
    let mut buf = [0u16; 1024];
    let mut size = size_of_val(&buf) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            &HSTRING::from(name),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return false;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).eq_ignore_ascii_case(&expected)
}

fn set_value(key: PCWSTR, name: &str, enabled: bool) -> Result<()> {
    let name = HSTRING::from(name);
    unsafe {
        if enabled {
            let Some(cmd) = this_exe() else {
                return Err(Error::new(E_FAIL, "this .exe's path is unknown"));
            };
            let data: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key,
                &name,
                REG_SZ.0,
                Some(data.as_ptr() as *const c_void),
                (data.len() * 2) as u32,
            )
            .ok()
        } else {
            match RegDeleteKeyValueW(HKEY_CURRENT_USER, key, &name) {
                e if e == ERROR_FILE_NOT_FOUND => Ok(()),
                e => e.ok(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::RegDeleteTreeW;

    /// A key of the tests' own, so a failing test can never leave a value in
    /// the real Run key (which Windows would then start at sign-in).
    const TEST_KEY: PCWSTR = w!("Software\\DeskFrame-test");

    fn test_value() -> String {
        format!("value-{}", std::process::id())
    }

    #[test]
    fn set_and_clear() {
        let name = test_value();
        set_value(TEST_KEY, &name, true).unwrap();
        assert!(is_set(TEST_KEY, &name));
        set_value(TEST_KEY, &name, false).unwrap();
        assert!(!is_set(TEST_KEY, &name));
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, TEST_KEY);
        }
    }

    #[test]
    fn clearing_a_missing_value_is_fine() {
        let name = test_value() + "-missing";
        set_value(TEST_KEY, &name, false).unwrap();
        assert!(!is_set(TEST_KEY, &name));
    }

    #[test]
    fn no_command_without_an_exe_path() {
        assert_eq!(command(None), None);
        assert_eq!(command(Some(Path::new(""))), None);
        assert_eq!(
            command(Some(Path::new(r"D:\Apps\Desk Frame\deskframe.exe"))).as_deref(),
            Some(r#""D:\Apps\Desk Frame\deskframe.exe""#)
        );
    }
}

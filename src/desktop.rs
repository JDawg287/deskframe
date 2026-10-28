//! Show Desktop detection. See `docs/notes/show-desktop.md` for the rule.

use std::sync::OnceLock;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetClassNameW, GetShellWindow, GetWindow, GetWindowLongW,
    GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GW_HWNDPREV, WS_EX_TOPMOST,
};

/// What the rule needs to know about the desktop right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DesktopSnapshot {
    /// The window hosting the desktop icons was found.
    pub host_found: bool,
    /// That window is visible.
    pub host_visible: bool,
    /// Our bottom-most marker window sits below the host in z-order.
    pub marker_below_host: bool,
}

/// True while Show Desktop (Win+D) is active.
pub fn is_show_desktop(s: &DesktopSnapshot) -> bool {
    s.host_found && s.host_visible && s.marker_below_host
}

/// Windows 11 24H2 (build 26100.2454+) keeps the icons under `Progman`.
/// The `GetCurrentMonitorTopologyId` export only exists from that build on.
pub fn uses_24h2_layout() -> bool {
    static LAYOUT: OnceLock<bool> = OnceLock::new();
    *LAYOUT.get_or_init(|| unsafe {
        GetModuleHandleW(w!("user32.dll"))
            .map(|m| GetProcAddress(m, s!("GetCurrentMonitorTopologyId")).is_some())
            .unwrap_or(false)
    })
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

fn has_defview(parent: HWND) -> bool {
    unsafe { FindWindowExW(Some(parent), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok() }
}

fn process_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Explorer's `Progman` window, if Explorer is running.
fn shell_window() -> Option<HWND> {
    let shell = unsafe { GetShellWindow() };
    (!shell.is_invalid() && class_name(shell) == "Progman").then_some(shell)
}

/// The window currently hosting the desktop icons, or `None` when there is no
/// separate host (pre-24H2 normal state) or Explorer is not running.
pub fn desktop_host() -> Option<HWND> {
    let shell = shell_window()?;
    if uses_24h2_layout() {
        return has_defview(shell).then_some(shell);
    }
    if has_defview(shell) {
        return None;
    }
    let shell_pid = process_of(shell);
    let mut after: Option<HWND> = None;
    loop {
        let worker = unsafe { FindWindowExW(None, after, w!("WorkerW"), PCWSTR::null()) }.ok()?;
        let visible = unsafe { IsWindowVisible(worker).as_bool() };
        if visible && process_of(worker) == shell_pid && has_defview(worker) {
            return Some(worker);
        }
        after = Some(worker);
    }
}

/// Reads the current desktop state relative to our marker window.
pub fn take_snapshot(marker: HWND) -> DesktopSnapshot {
    let Some(host) = desktop_host() else {
        return DesktopSnapshot::default();
    };
    let host_visible = unsafe { IsWindowVisible(host).as_bool() };
    let below = unsafe { FindWindowExW(None, Some(host), w!("DeskFrame.App"), PCWSTR::null()) };
    DesktopSnapshot {
        host_found: true,
        host_visible,
        marker_below_host: below.map(|h| h == marker).unwrap_or(false),
    }
}

/// The back-most always-on-top window above `host`, so frames can slot in
/// just below real always-on-top windows during Show Desktop.
pub fn backmost_topmost_above(host: HWND) -> Option<HWND> {
    let mut hwnd = host;
    loop {
        hwnd = unsafe { GetWindow(hwnd, GW_HWNDPREV) }.ok()?;
        if hwnd.is_invalid() {
            return None;
        }
        let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        if ex & WS_EX_TOPMOST.0 != 0 && class_name(hwnd) != "DeskFrame.Frame" {
            return Some(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_host_above_marker_is_show_desktop() {
        let s = DesktopSnapshot {
            host_found: true,
            host_visible: true,
            marker_below_host: true,
        };
        assert!(is_show_desktop(&s));
    }

    #[test]
    fn marker_above_host_is_not() {
        let s = DesktopSnapshot {
            host_found: true,
            host_visible: true,
            marker_below_host: false,
        };
        assert!(!is_show_desktop(&s));
    }

    #[test]
    fn hidden_host_is_not() {
        let s = DesktopSnapshot {
            host_found: true,
            host_visible: false,
            marker_below_host: true,
        };
        assert!(!is_show_desktop(&s));
    }

    #[test]
    fn missing_host_is_not() {
        // A detection failure may only ever leave frames hidden, never raise them.
        let s = DesktopSnapshot {
            host_found: false,
            host_visible: true,
            marker_below_host: true,
        };
        assert!(!is_show_desktop(&s));
    }
}

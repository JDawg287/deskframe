//! The notification-area (tray) icon: the app's menu lives here. Explorer
//! forgets tray icons when it restarts, so `add` is called again on
//! `TaskbarCreated`.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{LoadIconMetric, LIM_SMALL};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dlog;

/// Callback message for tray events (lparam low word = the event).
pub const WM_TRAY: u32 = WM_APP + 2;
pub const ID_ADD_FRAME: usize = 200;
pub const ID_LOCK_ALL: usize = 201;
pub const ID_AUTOSTART: usize = 202;
pub const ID_QUIT: usize = 203;

const TRAY_ID: u32 = 1;
/// Enter or space on the focused tray icon (NIN_SELECT | NINF_KEY).
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
/// Resource id of the app icon (res/app.rc).
const APP_ICON: u16 = 1;

/// Copies `text` into a fixed, NUL-terminated UTF-16 buffer (truncating).
fn fill<const N: usize>(buf: &mut [u16; N], text: &str) {
    for (slot, unit) in buf.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *slot = unit;
    }
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    }
}

/// Adds the icon (again, after an Explorer restart).
pub fn add(hwnd: HWND) {
    let mut nid = data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    unsafe {
        let hinst = GetModuleHandleW(None).ok().map(Into::into);
        nid.hIcon = LoadIconMetric(hinst, PCWSTR(APP_ICON as usize as *const u16), LIM_SMALL)
            .unwrap_or_default();
    }
    fill(&mut nid.szTip, "DeskFrame");
    unsafe {
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            // Already there (TaskbarCreated also fires when the taskbar's
            // DPI changes): refresh it so the icon matches the new size.
            if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                dlog!("tray icon not added");
            }
            let _ = DestroyIcon(nid.hIcon);
            return;
        }
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        let _ = DestroyIcon(nid.hIcon);
    }
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd));
    }
}

/// A notification from the tray icon.
pub fn notify(hwnd: HWND, title: &str, text: &str) {
    let mut nid = data(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    fill(&mut nid.szInfoTitle, title);
    fill(&mut nid.szInfo, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// True for the tray events that open the menu (left or right click, or
/// the keyboard), given the `WM_TRAY` lparam.
pub fn opens_menu(lp: LPARAM) -> bool {
    let event = (lp.0 & 0xFFFF) as u32;
    event == WM_CONTEXTMENU || event == NIN_SELECT || event == NIN_KEYSELECT
}

/// Shows the app menu at the cursor and returns the chosen command (0 = none).
pub fn menu(hwnd: HWND, lock_all: bool, autostart: bool) -> usize {
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    unsafe {
        let Ok(m) = CreatePopupMenu() else {
            return 0;
        };
        let _ = AppendMenuW(m, MF_STRING, ID_ADD_FRAME, w!("Add frame…"));
        let _ = AppendMenuW(m, MF_STRING | check(lock_all), ID_LOCK_ALL, w!("Lock all"));
        let _ = AppendMenuW(
            m,
            MF_STRING | check(autostart),
            ID_AUTOSTART,
            w!("Start with Windows"),
        );
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(m, MF_STRING, ID_QUIT, w!("Quit DeskFrame"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // The usual tray-menu dance: foreground first so the menu closes on
        // an outside click, a null message after so it can open again.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            m,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        cmd.0 as usize
    }
}

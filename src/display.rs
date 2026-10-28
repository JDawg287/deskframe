//! Monitors, their stable device IDs, and the display-change debounce
//! (caveat 4: wait ~1 s after the last `WM_DISPLAYCHANGE`).

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::SetTimer;

pub const TIMER_DISPLAY_SETTLED: usize = 3;
const SETTLE_MS: u32 = 1000;
const MONITORINFOF_PRIMARY: u32 = 1;
const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 1;

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    /// Stable monitor interface path, e.g. `\\?\DISPLAY#GSM5B09#...`.
    pub device_id: String,
    pub rect: RECT,
    pub work: RECT,
    pub primary: bool,
    pub dpi: u32,
}

/// (Re)starts the settle timer; repeated display changes keep pushing it back.
pub fn schedule_settle(hwnd: HWND) {
    unsafe { SetTimer(Some(hwnd), TIMER_DISPLAY_SETTLED, SETTLE_MS, None) };
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn dpi_of(mon: HMONITOR) -> u32 {
    let (mut x, mut y) = (96u32, 96u32);
    match unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) => x,
        Err(_) => 96,
    }
}

fn device_id(adapter_output: &[u16]) -> String {
    let mut dd = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        EnumDisplayDevicesW(
            PCWSTR(adapter_output.as_ptr()),
            0,
            &mut dd,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    };
    if ok.as_bool() {
        from_wide(&dd.DeviceID)
    } else {
        from_wide(adapter_output)
    }
}

unsafe extern "system" fn collect(mon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(mon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }
        .as_bool()
    {
        list.push(MonitorInfo {
            device_id: device_id(&info.szDevice),
            rect: info.monitorInfo.rcMonitor,
            work: info.monitorInfo.rcWork,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            dpi: dpi_of(mon),
        });
    }
    true.into()
}

pub fn monitors() -> Vec<MonitorInfo> {
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut list as *mut Vec<MonitorInfo> as isize),
        );
    }
    list
}

/// One line per monitor for the log.
pub fn describe(list: &[MonitorInfo]) -> String {
    list.iter()
        .map(|m| {
            format!(
                "[{}{} {}x{} at {},{} dpi {}]",
                if m.primary { "primary " } else { "" },
                m.device_id,
                m.rect.right - m.rect.left,
                m.rect.bottom - m.rect.top,
                m.rect.left,
                m.rect.top,
                m.dpi
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

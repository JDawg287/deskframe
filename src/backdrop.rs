//! Blur behind a frame (glass design, caveat 9).
//!
//! Windows 11's documented system backdrops (`DWMWA_SYSTEMBACKDROP_TYPE`)
//! switch to a flat colour while a window is inactive, and a frame is almost
//! never the active window (see `docs/notes/spike-results.md`). This uses
//! user32's undocumented `SetWindowCompositionAttribute` blur-behind accent
//! instead, looked up at runtime. If it is missing or fails, the frame draws
//! a stronger tint (`theme::TINT_ALPHA_SOLID`) and nothing else changes.
//! The blur covers the window's rectangle; only Windows' own rounded
//! corners clip it (`round_corners`).

use std::ffi::c_void;

use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::dlog;

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    gradient: u32,
    animation: u32,
}

#[repr(C)]
struct CompositionData {
    attrib: u32,
    data: *mut c_void,
    size: usize,
}

type SetWindowCompositionAttribute = unsafe extern "system" fn(HWND, *mut CompositionData) -> BOOL;

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_DISABLED: u32 = 0;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;

/// Asks DWM to blur what is behind `hwnd`. Returns true when it accepted.
pub fn enable(hwnd: HWND) -> bool {
    set_accent(hwnd, ACCENT_ENABLE_BLURBEHIND)
}

/// Turns the blur behind `hwnd` off again.
pub fn disable(hwnd: HWND) {
    set_accent(hwnd, ACCENT_DISABLED);
}

fn set_accent(hwnd: HWND, state: u32) -> bool {
    let set = unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else {
            return false;
        };
        let Some(f) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else {
            dlog!("blur: SetWindowCompositionAttribute not found");
            return false;
        };
        std::mem::transmute::<unsafe extern "system" fn() -> isize, SetWindowCompositionAttribute>(
            f,
        )
    };
    let mut policy = AccentPolicy {
        state,
        flags: 0,
        gradient: 0,
        animation: 0,
    };
    let mut data = CompositionData {
        attrib: WCA_ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut c_void,
        size: size_of::<AccentPolicy>(),
    };
    unsafe { set(hwnd, &mut data).as_bool() }
}

/// Rounds the frame's corners with Windows 11's own rounding, which also
/// clips the blur (a window region does not clip it on a layered window).
/// DWM's border is turned off: the frame draws its own hairline.
pub fn round_corners(hwnd: HWND) {
    let corner = DWMWCP_ROUND;
    let no_border = DWMWA_COLOR_NONE;
    unsafe {
        let rounded = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            size_of_val(&corner) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &no_border as *const _ as *const c_void,
            size_of_val(&no_border) as u32,
        );
        if let Err(e) = rounded {
            dlog!("rounded corners not available: {e}");
        }
    }
}

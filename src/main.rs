#![windows_subsystem = "windows"]

mod actions;
mod app_window;
mod autostart;
mod backdrop;
mod config;
mod desktop;
mod display;
mod drop_target;
mod folder;
mod frame;
mod grid;
mod groups;
mod log;
mod placement;
mod render;
mod settings_panel;
mod shell_icons;
mod theme;
mod tray;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::CreateMutexW;

/// Holds the named mutex for the life of the process; `None` if another
/// DeskFrame already owns it.
fn single_instance() -> Option<HANDLE> {
    unsafe {
        let handle = CreateMutexW(None, true, w!("Local\\DeskFrame.SingleInstance")).ok()?;
        (GetLastError() != ERROR_ALREADY_EXISTS).then_some(handle)
    }
}

fn main() {
    log::init();
    dlog!("DeskFrame {} starting", env!("CARGO_PKG_VERSION"));
    let Some(_instance) = single_instance() else {
        dlog!("another instance is already running; exiting");
        return;
    };
    unsafe {
        // COM in a single-threaded apartment, plus OLE drag and drop.
        let _ = OleInitialize(None);
    }
    let result = app_window::run();
    unsafe { OleUninitialize() };
    match result {
        Ok(()) => dlog!("exit"),
        Err(e) => {
            dlog!("fatal: {e}");
            std::process::exit(1);
        }
    }
}

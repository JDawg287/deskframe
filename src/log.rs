//! Debug log: every line goes to the debugger (DebugView) and to
//! `deskframe.log` next to the .exe, which `tools/gate.ps1` reads.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::SystemInformation::GetLocalTime;

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// Start a fresh log once it passes this size.
const MAX_LOG_BYTES: u64 = 1_000_000;

/// The folder holding the .exe; `config.json` and the log live here.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn init() {
    let path = exe_dir().join("deskframe.log");
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    let mut opts = OpenOptions::new();
    if too_big {
        opts.write(true).create(true).truncate(true);
    } else {
        opts.append(true).create(true);
    }
    let _ = LOG_FILE.set(Mutex::new(opts.open(path).ok()));
}

pub fn write(msg: &str) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {msg}\r\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
    let wide: Vec<u16> = format!("[DeskFrame] {line}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };
    if let Some(lock) = LOG_FILE.get() {
        if let Ok(mut file) = lock.lock() {
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

/// `dlog!("frame {} moved", id)` writes one timestamped log line.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}

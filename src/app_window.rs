//! The hidden app window: owns the frames, timers and system broadcasts, and
//! doubles as the bottom-most marker for Show Desktop detection.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::actions;
use crate::autostart;
use crate::config::{self, Config, FrameConfig};
use crate::desktop;
use crate::display;
use crate::dlog;
use crate::frame::{self, Frame};
use crate::render::Renderer;
use crate::settings_panel;
use crate::theme;
use crate::tray;

const APP_CLASS: PCWSTR = w!("DeskFrame.App");
const TIMER_DESKTOP: usize = 1;
const TIMER_DESKTOP_RECHECK: usize = 2;
const DESKTOP_POLL_MS: u32 = 250;
const RECHECK_MS: u32 = 60;
const WM_APP_FOREGROUND: u32 = WM_APP + 3;
/// A frame changed something worth keeping: write `config.json`.
const WM_APP_SAVE: u32 = WM_APP + 4;
/// Remove the frame whose id is in wparam, then save.
const WM_APP_REMOVE_FRAME: u32 = WM_APP + 5;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;

static APP_HWND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// "Lock all" from the tray: every frame behaves as locked.
static LOCK_ALL: AtomicBool = AtomicBool::new(false);
/// The Add frame folder dialog is open.
static PICKING: AtomicBool = AtomicBool::new(false);

struct App {
    // Boxed on purpose: each window keeps a raw pointer to its Frame, so a
    // Frame must never move when the Vec grows.
    #[allow(clippy::vec_box)]
    frames: Vec<Box<Frame>>,
    show_desktop: bool,
    /// App-wide settings from `config.json`; the frames' own settings live
    /// in `frames` and are collected when saving.
    settings: Config,
    /// Frames whose window could not be created this run: kept in the
    /// config so a passing problem does not erase them.
    failed: Vec<FrameConfig>,
    renderer: Rc<RefCell<Renderer>>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state unless it is already borrowed further up the
/// stack (Win32 re-entry); callers either retry later or skip.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut guard| guard.as_mut().map(f))
    })
}

pub fn app_hwnd() -> HWND {
    HWND(APP_HWND.load(Ordering::Relaxed))
}

/// Asks the app to write `config.json` (posted, so it runs after the
/// caller's message is done).
pub fn request_save() {
    unsafe {
        let _ = PostMessageW(Some(app_hwnd()), WM_APP_SAVE, WPARAM(0), LPARAM(0));
    }
}

/// Asks the app to remove frame `id` and save.
pub fn request_remove(id: u32) {
    unsafe {
        let _ = PostMessageW(
            Some(app_hwnd()),
            WM_APP_REMOVE_FRAME,
            WPARAM(id as usize),
            LPARAM(0),
        );
    }
}

/// True while "Lock all" is on.
pub fn lock_all() -> bool {
    LOCK_ALL.load(Ordering::Relaxed)
}

fn save(app: &App) {
    let cfg = Config {
        lock_all: lock_all(),
        frames: app
            .frames
            .iter()
            .map(|f| f.config())
            .chain(app.failed.iter().cloned())
            .collect(),
        ..app.settings.clone()
    };
    match config::save(&config::config_path(), &cfg) {
        Ok(()) => dlog!("config saved ({} frames)", cfg.frames.len()),
        Err(e) => dlog!("config not saved: {e}"),
    }
}

pub fn run() -> Result<()> {
    let pmv2 = unsafe {
        AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
        .as_bool()
    };
    theme::refresh();
    dlog!(
        "per-monitor-v2 dpi = {pmv2}; 24H2 desktop layout = {}; light = {}",
        desktop::uses_24h2_layout(),
        theme::light()
    );

    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None)?.into() };
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: APP_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err(Error::from_thread());
    }
    frame::register_class(hinst)?;
    settings_panel::register_class(hinst)?;

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            APP_CLASS,
            w!("DeskFrame"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )?
    };
    APP_HWND.store(hwnd.0, Ordering::Relaxed);
    TASKBAR_CREATED.store(
        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        Ordering::Relaxed,
    );

    let renderer = Rc::new(RefCell::new(Renderer::new()?));
    let mut settings = config::load(&config::config_path());
    LOCK_ALL.store(settings.lock_all, Ordering::Relaxed);
    let configs = std::mem::take(&mut settings.frames);
    let monitors = display::monitors();
    dlog!("monitors: {}", display::describe(&monitors));
    let mut frames = Vec::new();
    let mut failed = Vec::new();
    for cfg in configs {
        match Frame::create(cfg.clone(), &monitors, renderer.clone()) {
            Ok(f) => frames.push(f),
            Err(e) => {
                dlog!("frame {} not created: {e}", cfg.id);
                failed.push(cfg);
            }
        }
    }
    let empty = frames.is_empty();
    let read_only = settings.read_only;
    APP.with(|cell| {
        *cell.borrow_mut() = Some(App {
            frames,
            show_desktop: false,
            settings,
            failed,
            renderer,
        })
    });
    tray::add(hwnd);
    if read_only {
        // Otherwise the empty start would look like a first run, and the
        // user's changes would silently not be saved.
        tray::notify(
            hwnd,
            "DeskFrame can't save changes",
            "config.json could not be read or moved aside (another program may be using it). Close that program and restart DeskFrame.",
        );
    } else if empty {
        tray::notify(
            hwnd,
            "DeskFrame",
            "Right-click the tray icon to add a frame",
        );
    }

    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    unsafe { SetTimer(Some(hwnd), TIMER_DESKTOP, DESKTOP_POLL_MS, None) };
    reassert("start");

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !hook.is_invalid() {
            let _ = UnhookWinEvent(hook);
        }
    }
    Ok(())
}

unsafe extern "system" fn on_foreground(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    unsafe {
        let _ = PostMessageW(Some(app_hwnd()), WM_APP_FOREGROUND, WPARAM(0), LPARAM(0));
    }
}

/// Puts the marker back at the bottom and re-applies every frame's z-order.
fn reassert(reason: &str) {
    unsafe {
        let _ = SetWindowPos(
            app_hwnd(),
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    let done = with_app(|app| {
        app.show_desktop = desktop::is_show_desktop(&desktop::take_snapshot(app_hwnd()));
        apply_z(app);
        app.show_desktop
    });
    dlog!("reassert ({reason}): show desktop = {done:?}");
}

fn refresh_desktop_state() {
    with_app(|app| {
        let snap = desktop::take_snapshot(app_hwnd());
        let now = desktop::is_show_desktop(&snap);
        if now != app.show_desktop {
            app.show_desktop = now;
            dlog!("show desktop {} ({snap:?})", if now { "ON" } else { "OFF" });
            apply_z(app);
        }
    });
}

fn apply_z(app: &App) {
    if app.show_desktop {
        let after = desktop::desktop_host().and_then(desktop::backmost_topmost_above);
        for f in &app.frames {
            f.set_z(HWND_TOPMOST);
            if let Some(a) = after {
                f.set_z(a);
            }
        }
    } else {
        for f in &app.frames {
            f.set_z(HWND_BOTTOM);
        }
    }
}

/// Puts every frame back at its saved spot for the monitors present now.
fn restore_all() {
    let monitors = display::monitors();
    dlog!("monitors: {}", display::describe(&monitors));
    with_app(|app| app.frames.iter().for_each(|f| f.restore(&monitors)));
}

fn redraw_all() {
    with_app(|app| app.frames.iter().for_each(|f| f.redraw()));
}

/// Runs a tray menu command.
fn command(hwnd: HWND, id: usize) {
    match id {
        tray::ID_ADD_FRAME => {
            // The dialog is modal: tray clicks while it is open are ignored
            // so a second menu and dialog cannot open on top of it.
            PICKING.store(true, Ordering::Relaxed);
            let folder = actions::pick_folder(hwnd);
            PICKING.store(false, Ordering::Relaxed);
            if let Some(folder) = folder {
                add_frame(folder);
            }
        }
        tray::ID_LOCK_ALL => {
            let on = !lock_all();
            LOCK_ALL.store(on, Ordering::Relaxed);
            dlog!("lock all = {on}");
            with_app(|app| {
                app.frames.iter().for_each(|f| f.redraw());
                save(app);
            });
        }
        tray::ID_AUTOSTART => {
            let on = !autostart::is_enabled();
            match autostart::set(on) {
                Ok(()) => dlog!("start with Windows = {on}"),
                Err(e) => dlog!("start with Windows not changed: {e}"),
            }
        }
        tray::ID_QUIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

/// A new frame for `folder`, centred on the primary monitor, saved.
fn add_frame(folder: PathBuf) {
    let monitors = display::monitors();
    with_app(|app| {
        // Frames that failed to open this run still own their ids.
        let id = config::next_id(
            app.frames
                .iter()
                .map(|f| f.id())
                .chain(app.failed.iter().map(|c| c.id)),
        );
        let cfg = FrameConfig {
            id,
            folder,
            ..Default::default()
        };
        match Frame::create(cfg, &monitors, app.renderer.clone()) {
            Ok(f) => {
                dlog!("frame {id} added");
                app.frames.push(f);
                apply_z(app);
                save(app);
            }
            Err(e) => dlog!("frame {id} not created: {e}"),
        }
    });
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                match wp.0 {
                    TIMER_DESKTOP => refresh_desktop_state(),
                    TIMER_DESKTOP_RECHECK => {
                        let _ = KillTimer(Some(hwnd), TIMER_DESKTOP_RECHECK);
                        refresh_desktop_state();
                    }
                    display::TIMER_DISPLAY_SETTLED => {
                        let _ = KillTimer(Some(hwnd), display::TIMER_DISPLAY_SETTLED);
                        dlog!("displays settled");
                        restore_all();
                        reassert("display settled");
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_SAVE => {
                with_app(|app| save(app));
                LRESULT(0)
            }
            WM_ENDSESSION if wp.0 != 0 => {
                // Logoff or shutdown: an open settings panel saves only when
                // it closes, so save its changes now.
                dlog!("session ending: saving");
                with_app(|app| save(app));
                LRESULT(0)
            }
            WM_APP_REMOVE_FRAME => {
                let id = wp.0 as u32;
                dlog!("frame {id} removed");
                with_app(|app| {
                    app.frames.retain(|f| f.id() != id);
                    save(app);
                });
                LRESULT(0)
            }
            tray::WM_TRAY => {
                if tray::opens_menu(lp) && !PICKING.load(Ordering::Relaxed) {
                    let cmd = tray::menu(hwnd, lock_all(), autostart::is_enabled());
                    command(hwnd, cmd);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                command(hwnd, wp.0 & 0xFFFF);
                LRESULT(0)
            }
            WM_APP_FOREGROUND => {
                refresh_desktop_state();
                SetTimer(Some(hwnd), TIMER_DESKTOP_RECHECK, RECHECK_MS, None);
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                dlog!(
                    "WM_DISPLAYCHANGE {}x{} {}bpp",
                    lp.0 & 0xFFFF,
                    (lp.0 >> 16) & 0xFFFF,
                    wp.0
                );
                display::schedule_settle(hwnd);
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                if wp.0 == PBT_APMRESUMEAUTOMATIC {
                    restore_all();
                    reassert("resume");
                }
                LRESULT(1)
            }
            WM_SETTINGCHANGE => {
                if lp.0 != 0 {
                    let name = PCWSTR(lp.0 as *const u16).to_string().unwrap_or_default();
                    if name == "ImmersiveColorSet" && theme::refresh() {
                        dlog!("theme changed: light = {}", theme::light());
                        redraw_all();
                    }
                }
                LRESULT(0)
            }
            WM_DWMCOLORIZATIONCOLORCHANGED => {
                if theme::refresh() {
                    dlog!("accent colour changed");
                    redraw_all();
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                tray::remove(hwnd);
                let frames = APP.with(|cell| cell.borrow_mut().take());
                drop(frames);
                dlog!("quitting");
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                dlog!("explorer restarted (TaskbarCreated)");
                tray::add(hwnd);
                reassert("explorer restart");
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

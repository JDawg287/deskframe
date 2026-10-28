//! One frame: an unowned `WS_EX_TOOLWINDOW | WS_EX_LAYERED` popup that stays
//! at the bottom of the z-order unless the app window moves it on purpose.
//!
//! Win32 re-enters the window procedure during `SetWindowPos` and friends, so
//! the procedure only ever gets `&Frame` and all state is in `Cell`/`RefCell`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use windows::core::{w, Error, Result, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::RevokeDragDrop;
use windows::Win32::System::SystemServices::MK_LBUTTON;
use windows::Win32::UI::Controls::{
    TOOLTIPS_CLASSW, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTM_POP, TTM_SETMAXTIPWIDTH,
    TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    DragDetect, GetKeyState, TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, VK_DELETE,
    VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::Shell::IShellItemArray;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::actions;
use crate::app_window;
use crate::backdrop;
use crate::config::{FrameConfig, Placement};
use crate::display::{self, MonitorInfo};
use crate::dlog;
use crate::drop_target::{self, DropSpot};
use crate::folder::{self, Entry, FolderError, Watcher, WM_FOLDER_CHANGED, WM_FOLDER_LISTED};
use crate::grid::Layout;
use crate::groups;
use crate::placement::{self, Spot};
use crate::render::{FrameScene, GridScene, Renderer, Surface};
use crate::settings_panel::{Panel, WM_PANEL_CLOSED};
use crate::shell_icons::{self, IconKey, ThumbKey, WM_THUMBNAIL};
use crate::theme;

/// Posted to a frame when dropped items are waiting in `transfers`.
const WM_TRANSFER: u32 = WM_APP + 8;

pub const FRAME_CLASS: PCWSTR = w!("DeskFrame.Frame");
const MENU_OPEN: usize = 101;
const MENU_SHOW_IN_FOLDER: usize = 102;
const MENU_COPY_PATH: usize = 103;
const MENU_DELETE: usize = 104;
/// Width of the resize band along the frame's edges, in DIPs.
const RESIZE_BORDER: f32 = 6.0;
const TIMER_RELIST: usize = 1;
const TIMER_REWATCH: usize = 2;
/// After letting go of a folder on a drive being removed, look again this
/// much later (the drive is gone by then, or the removal was cancelled).
const REWATCH_MS: u32 = 3_000;
/// Quiet time after the last folder change before re-listing.
const RELIST_DEBOUNCE_MS: u32 = 150;

pub struct Frame {
    id: u32,
    hwnd: Cell<HWND>,
    title: String,
    folder: PathBuf,
    /// Saved position per monitor and the monitor last left on (config).
    placements: RefCell<BTreeMap<String, Placement>>,
    last_monitor: RefCell<Option<String>>,
    /// Inside `restore`'s own move (its DPI change must not undo the zoom).
    restoring: Cell<bool>,
    /// Inside a user drag or resize (between WM_ENTERSIZEMOVE and
    /// WM_EXITSIZEMOVE): the window rect when it started.
    size_move_start: Cell<Option<RECT>>,
    /// The folder's items, or why they cannot be shown.
    items: RefCell<std::result::Result<Vec<Entry>, FolderError>>,
    /// Numbers listing requests; only the newest answer is used.
    list_generation: Cell<u64>,
    /// When the newest listing was requested (for the log).
    list_started: Cell<Option<Instant>>,
    /// The first listing has arrived (until then the frame shows no message).
    listed: Cell<bool>,
    watcher: RefCell<Option<Watcher>>,
    /// The open settings panel, if any.
    panel: RefCell<Option<Box<Panel>>>,
    /// Icon per item, looked up when the item is first drawn.
    icon_keys: RefCell<Vec<Option<IconKey>>>,
    /// Items per type group and each group's header text; both empty when
    /// the frame shows no headers.
    group_sizes: RefCell<Vec<usize>>,
    group_titles: RefCell<Vec<String>>,
    /// The layout for (width px, scale); cleared when the items change.
    layout_cache: RefCell<Option<(f32, f32, Rc<Layout>)>>,
    /// Content scroll offset in physical px.
    scroll: Cell<f32>,
    /// The item under the mouse and the selected item (indexes into `items`).
    hover_item: Cell<Option<usize>>,
    selected: Cell<Option<usize>>,
    /// Where dragged files would land, while a drag is over the frame.
    drop: Cell<Option<DropSpot>>,
    /// Where the left button went down on an item (a drag starts once the
    /// mouse moves past the system's drag distance).
    /// The item's path, not its index: the listing can change before the
    /// drag starts.
    press: Cell<Option<(POINT, PathBuf)>>,
    /// The settings panel was open when the right button went down, so this
    /// right-click closes it rather than opening a new one.
    panel_at_press: Cell<bool>,
    /// Dropped items waiting to be copied or moved here (run from
    /// `WM_TRANSFER`, after the drop has returned to the source).
    transfers: RefCell<Vec<(IShellItemArray, PathBuf, bool)>>,
    renderer: Rc<RefCell<Renderer>>,
    surface: RefCell<Surface>,
    /// Expanded size in frame units (px = units × `scale`); equal to DIPs
    /// unless the frame is zoomed for another resolution.
    size_dip: Cell<(f32, f32)>,
    /// The window's DPI (Windows' rounded corners scale with it).
    dpi: Cell<u32>,
    /// Drawing scale: DPI / 96 normally, zoomed when the frame is shown at
    /// another resolution than it was placed at (Phase 5).
    scale: Cell<f32>,
    collapsed: Cell<bool>,
    locked: Cell<bool>,
    /// Mouse is over the frame (client or title area).
    hover: Cell<bool>,
    /// DWM accepted the blur backdrop.
    blurred: Cell<bool>,
    /// The frame's own appearance settings (config).
    opacity: Cell<f32>,
    blur: Cell<bool>,
    hide_extensions: Cell<bool>,
    /// Set only while `set_z` runs; otherwise every z-order change is dropped.
    allow_z: Cell<bool>,
    /// Windows tooltip showing the hovered item's full name, and the item
    /// and cell it is set to (so it is updated only when they change).
    tooltip: Cell<HWND>,
    tip_for: Cell<Option<(usize, RECT)>>,
}

pub fn register_class(hinst: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        lpszClassName: FRAME_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err(Error::from_thread());
    }
    Ok(())
}

fn lparam_point(lp: LPARAM) -> POINT {
    POINT {
        x: (lp.0 & 0xFFFF) as i16 as i32,
        y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32,
    }
}

impl Frame {
    /// Creates and shows the frame described by `cfg`, placed on the
    /// monitors present now.
    pub fn create(
        cfg: FrameConfig,
        monitors: &[MonitorInfo],
        renderer: Rc<RefCell<Renderer>>,
    ) -> Result<Box<Frame>> {
        // No monitor reported at all: default size at the screen origin.
        let spot = placement::resolve(&cfg, monitors).unwrap_or(Spot {
            rect: RECT {
                left: 0,
                top: 0,
                right: theme::DEFAULT_FRAME_W as i32,
                bottom: theme::DEFAULT_FRAME_H as i32,
            },
            scale: 1.0,
        });
        let rect = spot.rect;
        let dpi = placement::monitor_for(rect, monitors).map_or(96, |m| m.dpi);
        let s = spot.scale;
        let size_dip = (
            (rect.right - rect.left) as f32 / s,
            (rect.bottom - rect.top) as f32 / s,
        );
        let pos = POINT {
            x: rect.left,
            y: rect.top,
        };
        let folder = cfg.folder;
        // A drive root has no file name; show the path itself.
        let title = folder
            .file_name()
            .unwrap_or(folder.as_os_str())
            .to_string_lossy()
            .into_owned();
        let id = cfg.id;
        let frame = Box::new(Frame {
            id,
            hwnd: Cell::new(HWND::default()),
            title,
            folder,
            placements: RefCell::new(cfg.placements),
            last_monitor: RefCell::new(cfg.last_monitor),
            size_move_start: Cell::new(None),
            restoring: Cell::new(false),
            items: RefCell::new(Ok(Vec::new())),
            list_generation: Cell::new(0),
            list_started: Cell::new(None),
            listed: Cell::new(false),
            watcher: RefCell::new(None),
            panel: RefCell::new(None),
            icon_keys: RefCell::new(Vec::new()),
            group_sizes: RefCell::new(Vec::new()),
            group_titles: RefCell::new(Vec::new()),
            layout_cache: RefCell::new(None),
            scroll: Cell::new(0.0),
            hover_item: Cell::new(None),
            selected: Cell::new(None),
            press: Cell::new(None),
            transfers: RefCell::new(Vec::new()),
            panel_at_press: Cell::new(false),
            drop: Cell::new(None),
            renderer,
            surface: RefCell::new(Surface::new()),
            size_dip: Cell::new(size_dip),
            dpi: Cell::new(dpi),
            scale: Cell::new(s),
            collapsed: Cell::new(cfg.collapsed),
            locked: Cell::new(cfg.locked),
            hover: Cell::new(false),
            blurred: Cell::new(false),
            opacity: Cell::new(cfg.opacity),
            blur: Cell::new(cfg.blur),
            hide_extensions: Cell::new(cfg.hide_extensions),
            allow_z: Cell::new(false),
            tooltip: Cell::new(HWND::default()),
            tip_for: Cell::new(None),
        });
        let (w, h) = frame.size_px();
        unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                FRAME_CLASS,
                &HSTRING::from(&frame.title),
                WS_POPUP,
                pos.x,
                pos.y,
                w,
                h,
                None,
                None,
                Some(hinst),
                Some(&*frame as *const Frame as *const c_void),
            )?;
            let actual_dpi = GetDpiForWindow(hwnd);
            if actual_dpi != 0 {
                frame.dpi.set(actual_dpi);
            }
            backdrop::round_corners(hwnd);
            let blurred =
                frame.blur.get() && theme::transparency_enabled() && backdrop::enable(hwnd);
            frame.blurred.set(blurred);
            dlog!("frame {id}: blur behind = {blurred}");
            frame.set_z(HWND_BOTTOM);
            *frame.watcher.borrow_mut() = Some(Watcher::start(frame.folder.clone(), hwnd, id));
            if let Err(e) = drop_target::register(&frame) {
                dlog!("frame {id}: drop target not registered: {e}");
            }
            frame.create_tooltip(hinst);
            frame.relist();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        dlog!("frame {id} created at {},{} size {w}x{h}", pos.x, pos.y);
        Ok(frame)
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd.get()
    }

    /// One Windows tooltip for the frame. Its single tool covers the hovered
    /// cell (nothing while no item is hovered); `TTF_SUBCLASS` lets it watch
    /// the mouse itself, and `TTS_ALWAYSTIP` shows it although frames are
    /// never the active window.
    fn create_tooltip(&self, hinst: HINSTANCE) {
        let tip = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                TOOLTIPS_CLASSW,
                None,
                WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                Some(self.hwnd()),
                None,
                Some(hinst),
                None,
            )
        };
        let tip = match tip {
            Ok(t) => t,
            Err(e) => {
                dlog!("frame {}: no tooltip: {e}", self.id);
                return;
            }
        };
        self.tooltip.set(tip);
        let mut info = self.tool_info(RECT::default());
        let mut empty = [0u16];
        info.lpszText = PWSTR(empty.as_mut_ptr());
        unsafe {
            SendMessageW(
                tip,
                TTM_ADDTOOLW,
                None,
                Some(LPARAM(&info as *const _ as isize)),
            );
            // Long names wrap instead of running off the screen.
            SendMessageW(tip, TTM_SETMAXTIPWIDTH, None, Some(LPARAM(480)));
        }
    }

    fn tool_info(&self, rect: RECT) -> TTTOOLINFOW {
        TTTOOLINFOW {
            cbSize: size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_SUBCLASS,
            hwnd: self.hwnd(),
            uId: 1,
            rect,
            ..Default::default()
        }
    }

    /// Points the tooltip at the hovered item's cell, with its full name.
    fn sync_tooltip(&self) {
        let tip = self.tooltip.get();
        if tip.is_invalid() {
            return;
        }
        let want = self
            .hover_item
            .get()
            .and_then(|i| Some((i, self.cell_client_rect(i)?)));
        if want == self.tip_for.get() {
            return;
        }
        self.tip_for.set(want);
        let (rect, name) = match want {
            Some((i, r)) => (r, self.entry(i).map(|e| e.name).unwrap_or_default()),
            None => (RECT::default(), String::new()),
        };
        let mut text: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let mut info = self.tool_info(rect);
        info.lpszText = PWSTR(text.as_mut_ptr());
        unsafe {
            SendMessageW(tip, TTM_POP, None, None);
            SendMessageW(
                tip,
                TTM_NEWTOOLRECTW,
                None,
                Some(LPARAM(&info as *const _ as isize)),
            );
            SendMessageW(
                tip,
                TTM_UPDATETIPTEXTW,
                None,
                Some(LPARAM(&info as *const _ as isize)),
            );
        }
    }

    /// Item `i`'s cell in client px, cut to the content area.
    fn cell_client_rect(&self, i: usize) -> Option<RECT> {
        let c = self.layout()?.cell_rect(i);
        let top = theme::TITLE_H * self.scale();
        let y = top + c.y - self.scroll.get();
        Some(RECT {
            left: c.x as i32,
            top: y.max(top) as i32,
            right: (c.x + c.w) as i32,
            bottom: (y + c.h) as i32,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// This frame as it is saved in `config.json`.
    pub fn config(&self) -> FrameConfig {
        FrameConfig {
            id: self.id,
            folder: self.folder.clone(),
            collapsed: self.collapsed.get(),
            locked: self.locked.get(),
            placements: self.placements.borrow().clone(),
            last_monitor: self.last_monitor.borrow().clone(),
            opacity: self.opacity.get(),
            blur: self.blur.get(),
            hide_extensions: self.hide_extensions.get(),
        }
    }

    /// Puts the frame back at its saved spot for the monitors present now
    /// (start-up, display changes, resume). Never saves: Windows moving a
    /// frame is not the user moving it.
    pub fn restore(&self, monitors: &[MonitorInfo]) {
        let Some(spot) = placement::resolve(&self.config(), monitors) else {
            dlog!("frame {}: no monitors reported; left where it is", self.id);
            return;
        };
        let rect = spot.rect;
        if let Some(m) = placement::monitor_for(rect, monitors) {
            self.dpi.set(m.dpi);
        }
        self.set_scale(spot.scale);
        let s = spot.scale;
        self.size_dip.set((
            (rect.right - rect.left) as f32 / s,
            (rect.bottom - rect.top) as f32 / s,
        ));
        let (w, h) = self.size_px();
        // Moving onto a monitor with another DPI sends WM_DPICHANGED from
        // inside SetWindowPos; it must not replace the zoomed scale.
        self.restoring.set(true);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd(),
                None,
                rect.left,
                rect.top,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.restoring.set(false);
        self.clamp_scroll();
        self.redraw();
        dlog!(
            "frame {}: restored to {},{} {w}x{h}",
            self.id,
            rect.left,
            rect.top
        );
    }

    fn window_rect(&self) -> Option<RECT> {
        let mut r = RECT::default();
        unsafe { GetWindowRect(self.hwnd(), &mut r) }.ok()?;
        Some(r)
    }

    /// Records where the user left the frame, on the monitor holding it.
    fn remember_position(&self) {
        let Some(mut r) = self.window_rect() else {
            return;
        };
        // Saved height is the expanded one, also while collapsed.
        r.bottom = r.top + (self.size_dip.get().1 * self.scale()).round() as i32;
        let monitors = display::monitors();
        if let Some(mon) = placement::monitor_for(r, &monitors) {
            let spot = placement::to_relative(r, mon, self.scale());
            self.placements
                .borrow_mut()
                .insert(mon.device_id.clone(), spot);
            *self.last_monitor.borrow_mut() = Some(mon.device_id.clone());
            dlog!("frame {}: position {spot:?} on {}", self.id, mon.device_id);
        }
    }

    fn scale(&self) -> f32 {
        self.scale.get()
    }

    /// Changes the drawing scale, keeping the same part of the folder in view
    /// (the scroll offset is kept in px).
    fn set_scale(&self, s: f32) {
        let old = self.scale.replace(s);
        if old > 0.0 {
            self.scroll.set(self.scroll.get() * s / old);
        }
    }

    /// Size shown now in DIPs: the header only when collapsed.
    fn shown_dip(&self) -> (f32, f32) {
        let (w, h) = self.size_dip.get();
        if self.collapsed.get() {
            (w, theme::TITLE_H)
        } else {
            (w, h)
        }
    }

    /// Window size at drawing scale `s`, in physical px.
    fn size_px_at(&self, s: f32) -> (i32, i32) {
        let (w, h) = self.shown_dip();
        ((w * s).round() as i32, (h * s).round() as i32)
    }

    fn size_px(&self) -> (i32, i32) {
        self.size_px_at(self.scale())
    }

    /// Moves the frame in z-order: `HWND_BOTTOM`, `HWND_TOPMOST` or "after" a window.
    pub fn set_z(&self, after: HWND) {
        self.allow_z.set(true);
        let r = unsafe {
            SetWindowPos(
                self.hwnd(),
                Some(after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            )
        };
        self.allow_z.set(false);
        if let Err(e) = r {
            dlog!("frame {}: set_z failed: {e}", self.id);
        }
    }

    fn apply_size(&self) {
        let (w, h) = self.size_px();
        unsafe {
            let _ = SetWindowPos(
                self.hwnd(),
                None,
                0,
                0,
                w,
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.clamp_scroll();
        self.redraw();
    }

    /// Starts (or renews) leave tracking for the client or title area.
    fn track_leave(&self, nonclient: bool) {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: if nonclient {
                TME_LEAVE | TME_NONCLIENT
            } else {
                TME_LEAVE
            },
            hwndTrack: self.hwnd(),
            dwHoverTime: 0,
        };
        unsafe {
            let _ = TrackMouseEvent(&mut tme);
        }
    }

    /// Sets the hover highlights from where the pointer is now.
    fn refresh_hover(&self) {
        let inside = self.cursor_inside();
        let pt = self.cursor_point();
        let item = if inside { self.item_at(pt) } else { None };
        let changed =
            (self.hover.replace(inside) != inside) | (self.hover_item.replace(item) != item);
        if changed {
            self.redraw();
        }
    }

    fn cursor_inside(&self) -> bool {
        let mut pt = POINT::default();
        let mut r = RECT::default();
        unsafe {
            if GetCursorPos(&mut pt).is_err() || GetWindowRect(self.hwnd(), &mut r).is_err() {
                return false;
            }
        }
        pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
    }

    pub fn redraw(&self) {
        let (Ok(items), Ok(mut keys), Ok(titles)) = (
            self.items.try_borrow(),
            self.icon_keys.try_borrow_mut(),
            self.group_titles.try_borrow(),
        ) else {
            return;
        };
        let layout = self.layout();
        let grid = match (layout.as_deref(), items.as_ref()) {
            (Some(layout), Ok(entries)) => {
                let visible = layout.visible(self.scroll.get(), self.view_h_px());
                for i in visible.clone() {
                    if keys[i].is_none() {
                        keys[i] = Some(shell_icons::icon_key(&entries[i].path));
                    }
                }
                self.request_thumbnails(&entries[visible]);
                Some((layout, entries))
            }
            _ => None,
        };
        let scene = FrameScene {
            // Windows rounds the window at 8 DIP of its DPI; draw the same
            // curve in this frame's own units so the blur clip matches.
            corner_radius: theme::RADIUS * (self.dpi.get() as f32 / 96.0) / self.scale(),
            opacity: Some(self.opacity.get()).filter(|&o| o > 0.0),
            drop_frame: self.drop.get() == Some(DropSpot::Frame),
            title: self.title.clone(),
            collapsed: self.collapsed.get(),
            locked: self.is_locked(),
            light: theme::light(),
            hover: self.hover.get(),
            blurred: self.blurred.get(),
            message: match &*items {
                Ok(_) if !self.listed.get() => None,
                Ok(v) if v.is_empty() => Some("This folder is empty".to_string()),
                Ok(_) => None,
                Err(FolderError::NotFound) => Some("Folder not available".to_string()),
                Err(FolderError::Other(_)) => Some("Can't read this folder".to_string()),
            },
            grid: grid.map(|(layout, entries)| GridScene {
                entries,
                icons: &keys,
                layout,
                group_titles: &titles,
                scroll: self.scroll.get(),
                hover: self.hover_item.get(),
                selected: self.selected.get(),
                hide_extensions: self.hide_extensions.get(),
                drop_cell: match self.drop.get() {
                    Some(DropSpot::Cell(i)) => Some(i),
                    _ => None,
                },
            }),
        };
        let (Ok(mut renderer), Ok(mut surface)) = (
            self.renderer.try_borrow_mut(),
            self.surface.try_borrow_mut(),
        ) else {
            return;
        };
        if let Err(e) = renderer.draw_frame(
            self.hwnd(),
            &mut surface,
            self.size_px(),
            self.scale(),
            &scene,
        ) {
            dlog!("frame {}: draw failed: {e}", self.id);
        }
        drop((renderer, surface, items, keys, titles));
        self.sync_tooltip();
    }

    /// Asks for the previews of `shown` items that have none yet; each
    /// arrives as `WM_THUMBNAIL`.
    fn request_thumbnails(&self, shown: &[Entry]) {
        let Ok(mut renderer) = self.renderer.try_borrow_mut() else {
            return;
        };
        let px = theme::icon_px(self.scale());
        for key in shown.iter().filter_map(|e| ThumbKey::of(e, px)) {
            if renderer.wants_thumbnail(&key) {
                shell_icons::request_thumbnail(key, self.hwnd());
            }
        }
    }

    /// Cell layout of the expanded frame's items; `None` when collapsed or
    /// the folder cannot be listed.
    /// Built once per listing, width and scale (it is asked for on every
    /// mouse move).
    fn layout(&self) -> Option<Rc<Layout>> {
        if self.collapsed.get() {
            return None;
        }
        let n = self.items.try_borrow().ok()?.as_ref().ok()?.len();
        let s = self.scale();
        let w = self.size_dip.get().0 * s;
        let mut cache = self.layout_cache.try_borrow_mut().ok()?;
        if let Some((cw, cs, l)) = cache.as_ref() {
            if *cw == w && *cs == s {
                return Some(l.clone());
            }
        }
        let sizes = self.group_sizes.try_borrow().ok()?;
        let l = Rc::new(if sizes.is_empty() {
            Layout::new(w, n, s)
        } else {
            Layout::grouped(w, &sizes, s)
        });
        *cache = Some((w, s, l.clone()));
        Some(l)
    }

    /// Height of the content area (below the header) in physical px.
    fn view_h_px(&self) -> f32 {
        (self.shown_dip().1 - theme::TITLE_H) * self.scale()
    }

    fn clamp_scroll(&self) {
        let max = self
            .layout()
            .map_or(0.0, |l| l.max_scroll(self.view_h_px()));
        self.scroll.set(self.scroll.get().clamp(0.0, max));
    }

    /// The item under `pt` (window coordinates, physical px).
    fn item_at(&self, pt: POINT) -> Option<usize> {
        let top = theme::TITLE_H * self.scale();
        if (pt.y as f32) < top {
            return None;
        }
        self.layout()?
            .hit_test(pt.x as f32, pt.y as f32 - top, self.scroll.get())
    }

    fn entry(&self, i: usize) -> Option<Entry> {
        self.items.borrow().as_ref().ok()?.get(i).cloned()
    }

    fn cursor_point(&self) -> POINT {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(self.hwnd(), &mut pt);
        }
        pt
    }

    /// Asks for a fresh listing of the folder; it arrives as `WM_FOLDER_LISTED`.
    fn relist(&self) {
        let generation = self.list_generation.get() + 1;
        self.list_generation.set(generation);
        self.list_started.set(Some(Instant::now()));
        folder::list_in_background(self.folder.clone(), self.hwnd(), generation);
    }

    /// Shows a finished listing (older requests' answers are ignored).
    fn show_listing(&self, listing: folder::Listing) {
        if listing.generation != self.list_generation.get() {
            return;
        }
        let items = listing.items;
        if let Err(e) = &items {
            dlog!(
                "frame {}: {} not listed: {e:?}",
                self.id,
                self.folder.display()
            );
        }
        // Keep the selection on the same item if it is still there.
        let selected = self.selected.get().and_then(|i| self.entry(i));
        self.selected.set(
            selected.and_then(|old| items.as_ref().ok()?.iter().position(|e| e.path == old.path)),
        );
        // The pointer may still rest on an item; find it in the new list
        // below, once the items are in place.
        self.hover_item.set(None);
        // Same index, maybe another item: the tooltip text must be set again.
        self.tip_for.set(None);
        *self.icon_keys.borrow_mut() = vec![None; items.as_ref().map_or(0, Vec::len)];
        *self.group_sizes.borrow_mut() = listing.groups.iter().map(|g| g.len).collect();
        *self.group_titles.borrow_mut() = listing.groups.iter().map(groups::title).collect();
        *self.items.borrow_mut() = items;
        *self.layout_cache.borrow_mut() = None;
        self.listed.set(true);
        self.clamp_scroll();
        if self.hover.get() {
            self.hover_item.set(self.item_at(self.cursor_point()));
        }
        self.redraw();
        let n = self.items.borrow().as_ref().map_or(0, Vec::len);
        if let Some(started) = self.list_started.take() {
            let ms = started.elapsed().as_millis();
            dlog!("frame {}: {n} items listed and drawn in {ms} ms", self.id);
        }
    }

    /// `pt` (window coordinates, physical px) is on the title bar.
    fn in_title(&self, pt: POINT) -> bool {
        (pt.y as f32) < theme::TITLE_H * self.scale()
    }

    /// A click on the title bar: collapse or expand.
    fn toggle_collapsed(&self) {
        self.collapsed.set(!self.collapsed.get());
        dlog!("frame {}: collapsed = {}", self.id, self.collapsed.get());
        self.apply_size();
        app_window::request_save();
    }

    /// Locked: everything is content. Otherwise the edges resize (sides only
    /// while collapsed) and the title bar drags (a click on it collapses).
    fn hit_test(&self, screen: POINT) -> u32 {
        if self.is_locked() {
            return HTCLIENT;
        }
        let mut r = RECT::default();
        if unsafe { GetWindowRect(self.hwnd(), &mut r) }.is_err() {
            return HTCLIENT;
        }
        let local = POINT {
            x: screen.x - r.left,
            y: screen.y - r.top,
        };
        let band = (RESIZE_BORDER * self.scale()).round() as i32;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let left = local.x < band;
        let right = local.x >= w - band;
        let vertical = !self.collapsed.get();
        let top = vertical && local.y < band;
        let bottom = vertical && local.y >= h - band;
        match (left, right, top, bottom) {
            (true, _, true, _) => HTTOPLEFT,
            (_, true, true, _) => HTTOPRIGHT,
            (true, _, _, true) => HTBOTTOMLEFT,
            (_, true, _, true) => HTBOTTOMRIGHT,
            (true, ..) => HTLEFT,
            (_, true, ..) => HTRIGHT,
            (_, _, true, _) => HTTOP,
            (_, _, _, true) => HTBOTTOM,
            _ if self.in_title(local) => HTCAPTION,
            _ => HTCLIENT,
        }
    }

    /// Smallest size a resize may reach: one cell wide, one row tall (only
    /// the header while collapsed, which also fixes the height).
    fn min_max(&self, info: &mut MINMAXINFO) {
        let s = self.scale();
        let px = |dip: f32| (dip * s).round() as i32;
        info.ptMinTrackSize.x = px(theme::CELL_W + 2.0 * theme::CONTENT_PAD_X);
        if self.collapsed.get() {
            info.ptMinTrackSize.y = px(theme::TITLE_H);
            info.ptMaxTrackSize.y = px(theme::TITLE_H);
        } else {
            info.ptMinTrackSize.y = px(theme::TITLE_H
                + theme::CONTENT_PAD_TOP
                + theme::CELL_H
                + theme::CONTENT_PAD_BOTTOM);
        }
    }

    /// Shows a popup menu at the cursor and returns the chosen command (0 =
    /// none). Each entry is (command, text, enabled).
    fn popup(&self, commands: &[(usize, PCWSTR, bool)]) -> usize {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return 0;
            };
            for &(id, text, enabled) in commands {
                let flags = if enabled {
                    MF_STRING
                } else {
                    MF_STRING | MF_GRAYED
                };
                let _ = AppendMenuW(menu, flags, id, text);
            }
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd());
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                None,
                self.hwnd(),
                None,
            );
            let _ = DestroyMenu(menu);
            cmd.0 as usize
        }
    }

    /// Where files dropped at `screen` would go: a folder item under the
    /// pointer, else this frame's folder.
    pub fn drop_spot(&self, screen: POINT) -> (PathBuf, DropSpot) {
        let mut pt = screen;
        unsafe {
            let _ = ScreenToClient(self.hwnd(), &mut pt);
        }
        match self.item_at(pt).and_then(|i| Some((i, self.entry(i)?))) {
            Some((i, e)) if e.is_dir => (e.path, DropSpot::Cell(i)),
            _ => (self.folder.clone(), DropSpot::Frame),
        }
    }

    /// Shows (or clears) where a drag would drop.
    pub fn set_drop(&self, spot: Option<DropSpot>) {
        if self.drop.replace(spot) != spot {
            self.redraw();
        }
    }

    /// Starts dragging the pressed item out once the mouse has moved far
    /// enough with the left button held. True if a drag ran.
    fn drag_started(&self, wp: WPARAM, pt: POINT) -> bool {
        let Some((start, path)) = self.press.take() else {
            return false;
        };
        if wp.0 & MK_LBUTTON.0 as usize == 0 {
            return false;
        }
        let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
        if (pt.x - start.x).abs() <= dx && (pt.y - start.y).abs() <= dy {
            self.press.set(Some((start, path)));
            return false;
        }
        actions::drag_out(self.hwnd(), &path);
        // The pointer may have left during the drag without a leave message.
        self.refresh_hover();
        true
    }

    /// False while the folder is missing or unreadable ("Folder not
    /// available"): nothing can be dropped into it.
    pub fn folder_available(&self) -> bool {
        self.items.try_borrow().is_ok_and(|items| items.is_ok())
    }

    /// Queues dropped `items` to be copied or moved into `dest` once the
    /// drop has returned.
    pub fn queue_transfer(&self, items: IShellItemArray, dest: PathBuf, moving: bool) {
        self.transfers.borrow_mut().push((items, dest, moving));
        unsafe {
            let _ = PostMessageW(Some(self.hwnd()), WM_TRANSFER, WPARAM(0), LPARAM(0));
        }
    }

    /// Opens this frame's settings panel at the cursor, or only closes it if
    /// it was open when the right button went down (by then it has usually
    /// closed itself: the click took its focus).
    fn toggle_panel(&self) {
        let was_open = self.panel_at_press.take();
        self.close_panel();
        if was_open {
            return;
        }
        let mut at = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut at);
        }
        match Panel::open(self, at, self.renderer.clone()) {
            Ok(p) => *self.panel.borrow_mut() = Some(p),
            Err(e) => dlog!("frame {}: settings panel not opened: {e}", self.id),
        }
    }

    /// Closes the settings panel if open and saves what it changed.
    fn close_panel(&self) {
        if self.panel.borrow_mut().take().is_some() {
            app_window::request_save();
        }
    }

    pub fn dpi(&self) -> u32 {
        self.dpi.get()
    }

    /// The tint opacity in use: the frame's own, or the theme default.
    pub fn effective_opacity(&self) -> f32 {
        match self.opacity.get() {
            o if o > 0.0 => o,
            _ if self.blurred.get() => theme::TINT_ALPHA_BLUR,
            _ => theme::TINT_ALPHA_SOLID,
        }
    }

    pub fn set_opacity(&self, value: f32) {
        self.opacity
            .set(value.clamp(theme::OPACITY_MIN, theme::OPACITY_MAX));
        self.redraw();
    }

    pub fn blur(&self) -> bool {
        self.blur.get()
    }

    /// Turns the blur behind this frame on or off (on only takes effect
    /// while Windows' transparency effects are on).
    pub fn set_blur(&self, on: bool) {
        self.blur.set(on);
        let blurred = on && theme::transparency_enabled() && backdrop::enable(self.hwnd());
        if !blurred {
            backdrop::disable(self.hwnd());
        }
        self.blurred.set(blurred);
        dlog!("frame {}: blur = {on} (blurred = {blurred})", self.id);
        self.redraw();
    }

    pub fn hide_extensions(&self) -> bool {
        self.hide_extensions.get()
    }

    pub fn set_hide_extensions(&self, on: bool) {
        self.hide_extensions.set(on);
        self.redraw();
    }

    pub fn locked(&self) -> bool {
        self.locked.get()
    }

    pub fn set_locked(&self, on: bool) {
        self.locked.set(on);
        dlog!("frame {}: locked = {on}", self.id);
        self.redraw();
    }

    /// Locked by the frame's own setting or by "Lock all".
    fn is_locked(&self) -> bool {
        self.locked.get() || app_window::lock_all()
    }

    fn item_menu(&self, item: &Entry) {
        let cmd = self.popup(&[
            (MENU_OPEN, w!("Open"), true),
            (MENU_SHOW_IN_FOLDER, w!("Open containing folder"), true),
            (MENU_COPY_PATH, w!("Copy path"), true),
            (MENU_DELETE, w!("Delete"), true),
        ]);
        match cmd {
            MENU_OPEN => actions::open(self.hwnd(), &item.path),
            MENU_SHOW_IN_FOLDER => actions::show_in_folder(self.hwnd(), &item.path),
            MENU_COPY_PATH => actions::copy_text(self.hwnd(), &item.path.to_string_lossy()),
            MENU_DELETE => actions::delete(self.hwnd(), std::slice::from_ref(&item.path), false),
            _ => {}
        }
    }

    fn handle(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_WINDOWPOSCHANGING => {
                if !self.allow_z.get() {
                    let pos = unsafe { &mut *(lp.0 as *mut WINDOWPOS) };
                    pos.flags |= SWP_NOZORDER;
                }
                None
            }
            // Activate so apps we launch may take the foreground; the z-order
            // lock above keeps activation from raising the frame.
            WM_MOUSEACTIVATE => Some(LRESULT(MA_ACTIVATE as isize)),
            WM_NCHITTEST => Some(LRESULT(self.hit_test(lparam_point(lp)) as isize)),
            WM_NCLBUTTONDBLCLK => Some(LRESULT(0)),
            WM_MOUSEMOVE if self.drag_started(wp, lparam_point(lp)) => Some(LRESULT(0)),
            WM_MOUSEMOVE | WM_NCMOUSEMOVE => {
                self.track_leave(msg == WM_NCMOUSEMOVE);
                let client = msg == WM_MOUSEMOVE;
                let item = if client {
                    self.item_at(lparam_point(lp))
                } else {
                    None
                };
                let changed = !self.hover.replace(true) | (self.hover_item.replace(item) != item);
                if changed {
                    self.redraw();
                }
                if msg == WM_NCMOUSEMOVE {
                    return None;
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE | WM_NCMOUSELEAVE => {
                // Moving between the title and the content area also sends a
                // leave; only clear hover when the cursor really left.
                let inside = self.cursor_inside();
                let was = (self.hover.get(), self.hover_item.get());
                self.hover.set(inside);
                self.hover_item.set(None);
                if was != (inside, None) {
                    self.redraw();
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                self.press.set(None);
                // A locked frame's title is client area: a click there toggles.
                if self.in_title(lparam_point(lp)) {
                    self.toggle_collapsed();
                }
                Some(LRESULT(0))
            }
            WM_NCLBUTTONDOWN if wp.0 as u32 == HTCAPTION => {
                // Press on the title: moving past the drag distance moves the
                // frame as before; letting go first is a click, which toggles.
                let pt = lparam_point(lp);
                if unsafe { DragDetect(self.hwnd(), pt) }.as_bool() {
                    return None;
                }
                self.toggle_collapsed();
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                // A click in the content selects the item under it, or
                // clears the selection on empty space.
                let pt = lparam_point(lp);
                if pt.y as f32 >= theme::TITLE_H * self.scale() {
                    let item = self.item_at(pt);
                    self.press
                        .set(item.and_then(|i| Some((pt, self.entry(i)?.path))));
                    if self.selected.replace(item) != item {
                        self.redraw();
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDBLCLK => {
                if let Some(e) = self.item_at(lparam_point(lp)).and_then(|i| self.entry(i)) {
                    actions::open(self.hwnd(), &e.path);
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN if wp.0 == VK_RETURN.0 as usize => {
                if let Some(e) = self.selected.get().and_then(|i| self.entry(i)) {
                    actions::open(self.hwnd(), &e.path);
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN if wp.0 == VK_DELETE.0 as usize => {
                // Delete: Recycle Bin. Shift+Delete: permanently. The frame
                // updates through its folder watcher.
                if let Some(e) = self.selected.get().and_then(|i| self.entry(i)) {
                    let permanently = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                    actions::delete(self.hwnd(), std::slice::from_ref(&e.path), permanently);
                }
                Some(LRESULT(0))
            }
            WM_THUMBNAIL => {
                let mut done = unsafe { shell_icons::take_thumbnail(lp) };
                match self.renderer.try_borrow_mut() {
                    Ok(mut renderer) => {
                        let image = done.take_image();
                        renderer.thumbnail_ready(done.key.clone(), image);
                    }
                    // Drawing right now: take it once that is done.
                    Err(_) => {
                        shell_icons::repost_thumbnail(self.hwnd(), done);
                        return Some(LRESULT(0));
                    }
                }
                self.redraw();
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                // One row per notch (120); smooth wheels send smaller steps.
                let notches = ((wp.0 >> 16) as u16 as i16) as f32 / WHEEL_DELTA as f32;
                if let Some(layout) = self.layout() {
                    let next = layout.scroll_by(self.scroll.get(), -notches, self.view_h_px());
                    if next != self.scroll.get() {
                        self.scroll.set(next);
                        self.hover_item.set(self.item_at(self.cursor_point()));
                        self.redraw();
                    }
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN | WM_NCRBUTTONDOWN => {
                self.panel_at_press.set(self.panel.borrow().is_some());
                None
            }
            WM_RBUTTONUP => {
                match self.item_at(lparam_point(lp)) {
                    Some(i) => {
                        self.selected.set(Some(i));
                        self.redraw();
                        if let Some(e) = self.entry(i) {
                            self.item_menu(&e);
                        }
                    }
                    None => self.toggle_panel(),
                }
                Some(LRESULT(0))
            }
            WM_NCRBUTTONUP => {
                self.toggle_panel();
                Some(LRESULT(0))
            }
            WM_PANEL_CLOSED => {
                self.close_panel();
                Some(LRESULT(0))
            }
            WM_TRANSFER => {
                let jobs = std::mem::take(&mut *self.transfers.borrow_mut());
                for (items, dest, moving) in jobs {
                    actions::transfer(self.hwnd(), &items, &dest, moving);
                }
                Some(LRESULT(0))
            }
            WM_ENTERSIZEMOVE => {
                self.size_move_start.set(self.window_rect());
                None
            }
            WM_SIZE if self.size_move_start.get().is_some() => {
                // The user is resizing: the new size becomes the frame's size
                // (width only while collapsed; the expanded height is kept).
                let s = self.scale();
                let w = (lp.0 & 0xFFFF) as f32 / s;
                let h = ((lp.0 >> 16) & 0xFFFF) as f32 / s;
                let expanded_h = if self.collapsed.get() {
                    self.size_dip.get().1
                } else {
                    h
                };
                self.size_dip.set((w, expanded_h));
                self.clamp_scroll();
                self.redraw();
                Some(LRESULT(0))
            }
            WM_EXITSIZEMOVE => {
                // The only place a position is saved: the user just finished
                // dragging or resizing. A loop that moved nothing saves
                // nothing (it must not pin a frame Windows had placed elsewhere).
                let start = self.size_move_start.take();
                if matches!((start, self.window_rect()), (Some(a), Some(b)) if a != b) {
                    self.remember_position();
                    app_window::request_save();
                }
                Some(LRESULT(0))
            }
            WM_GETMINMAXINFO => {
                self.min_max(unsafe { &mut *(lp.0 as *mut MINMAXINFO) });
                Some(LRESULT(0))
            }
            WM_GETDPISCALEDSIZE => {
                // Size for the new DPI comes from our DIP size, not Windows' guess.
                let (w, h) = self.size_px_at(wp.0 as u32 as f32 / 96.0);
                let size = unsafe { &mut *(lp.0 as *mut SIZE) };
                size.cx = w;
                size.cy = h;
                Some(LRESULT(1))
            }
            WM_FOLDER_CHANGED => {
                // Restarting the timer coalesces a burst of changes into one re-list.
                unsafe { SetTimer(Some(self.hwnd()), TIMER_RELIST, RELIST_DEBOUNCE_MS, None) };
                Some(LRESULT(0))
            }
            WM_DEVICECHANGE if wp.0 as u32 == DBT_DEVICEQUERYREMOVE => {
                // The drive holding this folder is about to be removed (the
                // watcher registered for this warning). Let go of the folder,
                // whose open handle would block "Safely Remove".
                dlog!(
                    "frame {}: drive removal requested; releasing the folder",
                    self.id
                );
                // Waits only for the handle to close (at most a second),
                // never for the watcher thread itself.
                if let Some(w) = self.watcher.borrow_mut().take() {
                    w.release();
                }
                unsafe { SetTimer(Some(self.hwnd()), TIMER_REWATCH, REWATCH_MS, None) };
                Some(LRESULT(1))
            }
            WM_TIMER if wp.0 == TIMER_REWATCH => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd()), TIMER_REWATCH);
                }
                *self.watcher.borrow_mut() =
                    Some(Watcher::start(self.folder.clone(), self.hwnd(), self.id));
                self.relist();
                Some(LRESULT(0))
            }
            WM_FOLDER_LISTED => {
                // Safety: this lparam was made by folder::list_in_background.
                let listing = unsafe { folder::take_listing(lp) };
                self.show_listing(*listing);
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER_RELIST => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd()), TIMER_RELIST);
                }
                self.relist();
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                let dpi = (wp.0 & 0xFFFF) as u32;
                let dragging = self.size_move_start.get().is_some();
                let response = dpi_response(self.restoring.get(), dragging);
                dlog!(
                    "frame {}: dpi {} -> {dpi} ({response:?})",
                    self.id,
                    self.dpi.get()
                );
                self.dpi.set(dpi);
                match response {
                    DpiResponse::Note => self.redraw(),
                    DpiResponse::FollowDpi => {
                        self.set_scale(dpi as f32 / 96.0);
                        self.apply_size();
                    }
                    DpiResponse::Restore => self.restore(&display::monitors()),
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // Stop the watcher first, so it does not post to a window that is
        // being destroyed.
        self.watcher.get_mut().take();
        // The drop target points at this frame too.
        if !self.hwnd().is_invalid() {
            unsafe {
                let _ = RevokeDragDrop(self.hwnd());
            }
        }
        // The panel points at this frame: it must go first.
        self.panel.get_mut().take();
        let tip = self.tooltip.get();
        if !tip.is_invalid() {
            unsafe {
                let _ = DestroyWindow(tip);
            }
        }
        let hwnd = self.hwnd();
        if !hwnd.is_invalid() {
            unsafe {
                // Answers already queued for this window would never be
                // read: take them so their bitmaps and listings are freed.
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, Some(hwnd), WM_THUMBNAIL, WM_THUMBNAIL, PM_REMOVE)
                    .as_bool()
                {
                    drop(shell_icons::take_thumbnail(msg.lParam));
                }
                while PeekMessageW(
                    &mut msg,
                    Some(hwnd),
                    WM_FOLDER_LISTED,
                    WM_FOLDER_LISTED,
                    PM_REMOVE,
                )
                .as_bool()
                {
                    drop(folder::take_listing(msg.lParam));
                }
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            let frame = cs.lpCreateParams as *const Frame;
            (*frame).hwnd.set(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, frame as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Frame;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            (*ptr).hwnd.set(HWND::default());
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        match (*ptr).handle(msg, wp, lp) {
            Some(r) => r,
            None => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// What a frame does when Windows says its DPI changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DpiResponse {
    /// Only record the window's new DPI.
    Note,
    /// Draw at the new DPI (the user is dragging to another monitor).
    FollowDpi,
    /// Recompute the saved, zoomed spot for the monitors now present.
    Restore,
}

fn dpi_response(restoring: bool, dragging: bool) -> DpiResponse {
    if restoring {
        DpiResponse::Note
    } else if dragging {
        DpiResponse::FollowDpi
    } else {
        DpiResponse::Restore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_change_during_restore_only_notes_it() {
        // Restore's own move set the zoomed size; a DPI reset would undo it.
        assert_eq!(dpi_response(true, false), DpiResponse::Note);
    }

    #[test]
    fn dpi_change_while_dragging_follows_windows() {
        assert_eq!(dpi_response(false, true), DpiResponse::FollowDpi);
    }

    #[test]
    fn dpi_change_otherwise_restores_the_zoomed_spot() {
        // A resolution or scaling change: keep the frame's share of the screen.
        assert_eq!(dpi_response(false, false), DpiResponse::Restore);
    }
}

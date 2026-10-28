//! A frame's settings panel: a small glass popup opened by right-clicking
//! the frame's title, with an opacity slider and switches. `Layout` is pure
//! (DIPs from the panel's top-left corner); `Panel` is the window.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    VIRTUAL_KEY, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app_window;
use crate::backdrop;
use crate::display;
use crate::dlog;
use crate::frame::Frame;
use crate::grid::RectF;
use crate::placement;
use crate::render::{Renderer, Surface};
use crate::theme;

/// The panel's controls, in Tab order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Opacity,
    Blur,
    HideExtensions,
    Lock,
    Remove,
}

pub const CONTROLS: [Control; 5] = [
    Control::Opacity,
    Control::Blur,
    Control::HideExtensions,
    Control::Lock,
    Control::Remove,
];

pub const PANEL_W: f32 = 264.0;
const PAD: f32 = 8.0;
const TITLE_H: f32 = 36.0;
const LABEL_H: f32 = 22.0;
const SLIDER_H: f32 = 32.0;
const ROW_H: f32 = 36.0;
/// Space before the Remove row.
const GAP: f32 = 8.0;
/// Inset of the slider track from the panel's sides.
const TRACK_INSET: f32 = 20.0;

/// Where everything sits in the panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub w: f32,
    pub h: f32,
    pub title: RectF,
    /// "Opacity" and its value, above the slider.
    pub opacity_label: RectF,
    /// The slider's row (the track runs from `track_x0` to `track_x1`).
    pub slider: RectF,
    pub track_x0: f32,
    pub track_x1: f32,
    /// Rows for Blur, Hide extensions and Lock.
    pub switches: [RectF; 3],
    pub remove: RectF,
}

impl Layout {
    pub fn new() -> Layout {
        let w = PANEL_W;
        let row = |y: f32, h: f32| RectF {
            x: PAD,
            y,
            w: w - 2.0 * PAD,
            h,
        };
        let title = row(PAD, TITLE_H);
        let opacity_label = row(title.y + title.h, LABEL_H);
        let slider = row(opacity_label.y + opacity_label.h, SLIDER_H);
        let first = slider.y + slider.h;
        let switches = [0.0, 1.0, 2.0].map(|i| row(first + i * ROW_H, ROW_H));
        let remove = row(first + 3.0 * ROW_H + GAP, ROW_H);
        Layout {
            w,
            h: remove.y + remove.h + PAD,
            title,
            opacity_label,
            slider,
            track_x0: TRACK_INSET,
            track_x1: w - TRACK_INSET,
            switches,
            remove,
        }
    }

    /// The control under (`x`, `y`) in DIPs, or `None`.
    pub fn hit(&self, x: f32, y: f32) -> Option<Control> {
        let inside = |r: RectF| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        if inside(self.opacity_label) {
            return Some(Control::Opacity);
        }
        CONTROLS.into_iter().find(|&c| inside(self.rect(c)))
    }

    /// The opacity for a slider position (clamped to the track).
    pub fn slider_value(&self, x: f32) -> f32 {
        let t = ((x - self.track_x0) / (self.track_x1 - self.track_x0)).clamp(0.0, 1.0);
        theme::OPACITY_MIN + t * (theme::OPACITY_MAX - theme::OPACITY_MIN)
    }

    /// The slider position for an opacity.
    pub fn slider_x(&self, value: f32) -> f32 {
        let t = (value - theme::OPACITY_MIN) / (theme::OPACITY_MAX - theme::OPACITY_MIN);
        self.track_x0 + t.clamp(0.0, 1.0) * (self.track_x1 - self.track_x0)
    }

    /// The row rectangle of `control`.
    pub fn rect(&self, control: Control) -> RectF {
        match control {
            Control::Opacity => self.slider,
            Control::Blur => self.switches[0],
            Control::HideExtensions => self.switches[1],
            Control::Lock => self.switches[2],
            Control::Remove => self.remove,
        }
    }
}

/// What the panel shows right now (read from its frame on every draw).
pub struct PanelScene {
    pub light: bool,
    pub blurred: bool,
    /// The frame's tint opacity as used now (its own, or the default).
    pub opacity: f32,
    pub blur: bool,
    pub hide_extensions: bool,
    pub locked: bool,
    /// "Lock all" is on: the Lock switch shows on and cannot be changed.
    pub lock_all: bool,
    pub hover: Option<Control>,
    /// Keyboard focus ring, shown once Tab or an arrow key is used.
    pub focus: Option<Control>,
}

pub const PANEL_CLASS: PCWSTR = w!("DeskFrame.Settings");
/// Posted to the frame when its panel should close (the panel cannot drop
/// itself from inside its own window procedure).
pub const WM_PANEL_CLOSED: u32 = WM_APP + 7;
/// Opacity change per arrow key press or wheel notch.
const STEP: f32 = 0.05;

pub fn register_class(hinst: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        lpszClassName: PANEL_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err(Error::from_thread());
    }
    Ok(())
}

/// The settings panel of one frame. The frame owns it and drops it when
/// `WM_PANEL_CLOSED` arrives; the panel only reads and changes the frame.
pub struct Panel {
    hwnd: Cell<HWND>,
    frame: *const Frame,
    layout: Layout,
    /// Pixels per DIP on the panel's own monitor.
    scale: Cell<f32>,
    hover: Cell<Option<Control>>,
    focus: Cell<Option<Control>>,
    dragging: Cell<bool>,
    tracking_mouse: Cell<bool>,
    surface: RefCell<Surface>,
    renderer: Rc<RefCell<Renderer>>,
}

impl Panel {
    /// Opens `frame`'s panel with its top-left corner near `at` (screen px),
    /// kept inside that monitor's work area.
    pub fn open(frame: &Frame, at: POINT, renderer: Rc<RefCell<Renderer>>) -> Result<Box<Panel>> {
        let monitors = display::monitors();
        let spot = RECT {
            left: at.x,
            top: at.y,
            right: at.x + 1,
            bottom: at.y + 1,
        };
        let monitor = placement::monitor_for(spot, &monitors);
        // Sized for the monitor it opens on, which need not be the frame's.
        let dpi = monitor
            .map(|m| m.dpi)
            .filter(|&d| d > 0)
            .unwrap_or(frame.dpi());
        let scale = dpi as f32 / 96.0;
        let layout = Layout::new();
        let (w, h) = (
            (layout.w * scale).round() as i32,
            (layout.h * scale).round() as i32,
        );
        let (x, y) = match monitor {
            Some(m) => (
                at.x.clamp(m.work.left, (m.work.right - w).max(m.work.left)),
                at.y.clamp(m.work.top, (m.work.bottom - h).max(m.work.top)),
            ),
            None => (at.x, at.y),
        };
        let panel = Box::new(Panel {
            hwnd: Cell::new(HWND::default()),
            frame,
            layout,
            scale: Cell::new(scale),
            hover: Cell::new(None),
            focus: Cell::new(None),
            dragging: Cell::new(false),
            tracking_mouse: Cell::new(false),
            surface: RefCell::new(Surface::new()),
            renderer,
        });
        unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TOPMOST,
                PANEL_CLASS,
                w!("Frame settings"),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(hinst),
                Some(&*panel as *const Panel as *const c_void),
            )?;
            backdrop::round_corners(hwnd);
            if theme::transparency_enabled() {
                backdrop::enable(hwnd);
            }
            panel.redraw();
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }
        Ok(panel)
    }

    fn frame(&self) -> &Frame {
        // Safety: the frame owns this panel and drops it before itself.
        unsafe { &*self.frame }
    }

    fn scene(&self) -> PanelScene {
        let f = self.frame();
        PanelScene {
            light: theme::light(),
            blurred: theme::transparency_enabled(),
            opacity: f.effective_opacity(),
            blur: f.blur(),
            hide_extensions: f.hide_extensions(),
            locked: f.locked(),
            lock_all: app_window::lock_all(),
            hover: self.hover.get(),
            focus: self.focus.get(),
        }
    }

    fn redraw(&self) {
        let scene = self.scene();
        let scale = self.scale.get();
        let (w, h) = (
            (self.layout.w * scale).round() as i32,
            (self.layout.h * scale).round() as i32,
        );
        let (Ok(mut renderer), Ok(mut surface)) = (
            self.renderer.try_borrow_mut(),
            self.surface.try_borrow_mut(),
        ) else {
            return;
        };
        if let Err(e) = renderer.draw_panel(
            self.hwnd.get(),
            &mut surface,
            (w, h),
            scale,
            &self.layout,
            &scene,
        ) {
            dlog!("settings panel: draw failed: {e}");
        }
    }

    /// (`lp` client px) → panel DIPs.
    fn dip(&self, lp: LPARAM) -> (f32, f32) {
        let scale = self.scale.get();
        let x = (lp.0 & 0xFFFF) as i16 as f32 / scale;
        let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / scale;
        (x, y)
    }

    fn close(&self) {
        unsafe {
            let _ = PostMessageW(
                Some(self.frame().hwnd()),
                WM_PANEL_CLOSED,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    fn set_opacity(&self, value: f32) {
        self.frame().set_opacity(value);
        self.redraw();
    }

    /// Toggles a switch or runs Remove frame.
    fn activate(&self, control: Control) {
        let f = self.frame();
        match control {
            Control::Opacity => return,
            Control::Blur => f.set_blur(!f.blur()),
            Control::HideExtensions => f.set_hide_extensions(!f.hide_extensions()),
            Control::Lock if app_window::lock_all() => return,
            Control::Lock => f.set_locked(!f.locked()),
            Control::Remove => {
                self.close();
                app_window::request_remove(f.id());
                return;
            }
        }
        self.redraw();
    }

    fn handle(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => {
                // A click anywhere else closes the panel.
                self.close();
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                // Moved to a monitor with other scaling: take its size there.
                self.scale.set((wp.0 & 0xFFFF) as f32 / 96.0);
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd.get(),
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.redraw();
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if !self.tracking_mouse.get() {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd.get(),
                        dwHoverTime: 0,
                    };
                    if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
                        self.tracking_mouse.set(true);
                    }
                }
                let (x, y) = self.dip(lp);
                if self.dragging.get() {
                    self.set_opacity(self.layout.slider_value(x));
                }
                let over = self.layout.hit(x, y);
                if self.hover.replace(over) != over {
                    self.redraw();
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking_mouse.set(false);
                if self.hover.replace(None).is_some() {
                    self.redraw();
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = self.dip(lp);
                self.focus.set(None);
                match self.layout.hit(x, y) {
                    Some(Control::Opacity) => {
                        self.dragging.set(true);
                        unsafe { SetCapture(self.hwnd.get()) };
                        self.set_opacity(self.layout.slider_value(x));
                    }
                    Some(c) => self.activate(c),
                    None => self.redraw(),
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                if self.dragging.replace(false) {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                Some(LRESULT(0))
            }
            WM_CAPTURECHANGED => {
                self.dragging.set(false);
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let notches = ((wp.0 >> 16) as u16 as i16) as f32 / WHEEL_DELTA as f32;
                self.set_opacity(self.frame().effective_opacity() + notches * STEP);
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                let key = VIRTUAL_KEY(wp.0 as u16);
                match key {
                    VK_ESCAPE => self.close(),
                    VK_TAB => {
                        let back = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                        let i = self
                            .focus
                            .get()
                            .and_then(|f| CONTROLS.iter().position(|&c| c == f));
                        let n = CONTROLS.len();
                        let next = match (i, back) {
                            (None, false) => 0,
                            (None, true) => n - 1,
                            (Some(i), false) => (i + 1) % n,
                            (Some(i), true) => (i + n - 1) % n,
                        };
                        self.focus.set(Some(CONTROLS[next]));
                        self.redraw();
                    }
                    VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN => {
                        self.focus.set(Some(Control::Opacity));
                        let up = key == VK_RIGHT || key == VK_UP;
                        let step = if up { STEP } else { -STEP };
                        self.set_opacity(self.frame().effective_opacity() + step);
                    }
                    VK_SPACE | VK_RETURN => {
                        if let Some(c) = self.focus.get() {
                            self.activate(c);
                        }
                    }
                    _ => return None,
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        let hwnd = self.hwnd.get();
        if !hwnd.is_invalid() {
            unsafe {
                // Unhook first: destroying the window deactivates it, and
                // the frame that owns this panel may be going away too.
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            let panel = cs.lpCreateParams as *const Panel;
            (*panel).hwnd.set(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, panel as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Panel;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn centre(r: RectF) -> (f32, f32) {
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    #[test]
    fn slider_maps_ends_to_min_and_max() {
        let l = Layout::new();
        assert_eq!(l.slider_value(l.track_x0), theme::OPACITY_MIN);
        assert_eq!(l.slider_value(l.track_x1), theme::OPACITY_MAX);
        assert_eq!(l.slider_value(-100.0), theme::OPACITY_MIN);
        assert_eq!(l.slider_value(10_000.0), theme::OPACITY_MAX);
    }

    #[test]
    fn slider_round_trip() {
        let l = Layout::new();
        for v in [0.2, 0.45, 0.85, 1.0] {
            assert!((l.slider_value(l.slider_x(v)) - v).abs() < 1e-5, "{v}");
        }
    }

    #[test]
    fn hit_finds_each_control() {
        let l = Layout::new();
        for c in CONTROLS {
            let (x, y) = centre(l.rect(c));
            assert_eq!(l.hit(x, y), Some(c), "{c:?}");
        }
        // The label above the slider belongs to the slider.
        let (x, y) = centre(l.opacity_label);
        assert_eq!(l.hit(x, y), Some(Control::Opacity));
    }

    #[test]
    fn hit_outside_is_none() {
        let l = Layout::new();
        let (x, y) = centre(l.title);
        assert_eq!(l.hit(x, y), None);
        assert_eq!(l.hit(-1.0, 50.0), None);
        assert_eq!(l.hit(10.0, l.h + 1.0), None);
    }

    #[test]
    fn rows_fit_inside_the_panel() {
        let l = Layout::new();
        assert_eq!(l.w, PANEL_W);
        for c in CONTROLS {
            let r = l.rect(c);
            assert!(r.y + r.h <= l.h && r.x + r.w <= l.w, "{c:?}");
        }
    }
}

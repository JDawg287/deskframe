//! Direct2D/DirectWrite drawing into a 32-bpp premultiplied DIB, pushed to a
//! layered window with `UpdateLayeredWindow`. All drawing is in DIPs; the
//! render target's DPI does the scaling.

use windows::core::{w, Error, Result, HSTRING};
use windows::Win32::Foundation::{COLORREF, D2DERR_RECREATE_TARGET, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1LinearGradientBrush,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ANTIALIAS_MODE_ALIASED,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_EXTEND_MODE_CLAMP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_GAMMA_2_2,
    D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_LINE_SPACING_METHOD_UNIFORM,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_EMERGENCY_BREAK,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::folder::{self, Entry};
use crate::grid::{self, Layout};
use crate::settings_panel::{self as panel, Control};
use crate::shell_icons::{IconCache, IconKey, ThumbKey};
use crate::theme::{self, Rgba};

/// What one frame looks like right now.
pub struct FrameScene<'a> {
    pub title: String,
    pub collapsed: bool,
    pub locked: bool,
    pub light: bool,
    /// The mouse is over the frame: show the lock (if locked).
    pub hover: bool,
    /// DWM blurs what is behind the frame, so the tint can be lighter.
    pub blurred: bool,
    /// The frame's own tint opacity; `None` = the theme default.
    pub opacity: Option<f32>,
    /// Files are being dragged over the frame's own folder.
    pub drop_frame: bool,
    /// Frame corner radius in this frame's DIPs.
    pub corner_radius: f32,
    /// Centered text in the content area ("Folder not available").
    pub message: Option<String>,
    /// The icon grid; `None` when collapsed or the folder cannot be shown.
    pub grid: Option<GridScene<'a>>,
}

/// The folder's items as the frame shows them.
pub struct GridScene<'a> {
    pub entries: &'a [Entry],
    /// Icon per entry; the frame looks up the visible ones before drawing.
    pub icons: &'a [Option<IconKey>],
    /// Cell positions in physical px, below the header.
    pub layout: &'a Layout,
    /// One header per type group (`layout`'s groups); empty when the frame
    /// shows no headers.
    pub group_titles: &'a [String],
    /// Scroll offset in physical px.
    pub scroll: f32,
    pub hover: Option<usize>,
    pub selected: Option<usize>,
    /// Show file names without their extension.
    pub hide_extensions: bool,
    /// The folder item files are being dragged onto.
    pub drop_cell: Option<usize>,
}

/// A memory DC with a top-down 32-bpp DIB selected into it. One per frame.
pub struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl Surface {
    pub fn new() -> Self {
        Surface {
            dc: HDC::default(),
            bitmap: HBITMAP::default(),
            old: HGDIOBJ::default(),
            w: 0,
            h: 0,
        }
    }

    fn ensure(&mut self, w: i32, h: i32) -> Result<()> {
        if self.w == w && self.h == h && !self.dc.is_invalid() {
            return Ok(());
        }
        self.release();
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(Error::from_thread());
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(b) => b,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            self.old = SelectObject(dc, HGDIOBJ(bitmap.0));
            self.dc = dc;
            self.bitmap = bitmap;
            self.w = w;
            self.h = h;
        }
        Ok(())
    }

    fn release(&mut self) {
        if self.dc.is_invalid() {
            return;
        }
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
        // Reset field by field: assigning a whole `Surface` would drop the old
        // value, and its `Drop` would call `release` again, forever.
        self.dc = HDC::default();
        self.bitmap = HBITMAP::default();
        self.old = HGDIOBJ::default();
        self.w = 0;
        self.h = 0;
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.release();
    }
}

pub struct Renderer {
    d2d: ID2D1Factory,
    /// Measures header text (where its rule starts).
    dw: IDWriteFactory,
    target: Option<ID2D1DCRenderTarget>,
    title_fmt: IDWriteTextFormat,
    message_fmt: IDWriteTextFormat,
    glyph_fmt: IDWriteTextFormat,
    label_fmt: IDWriteTextFormat,
    /// Type-group headers: one line, ellipsis.
    group_fmt: IDWriteTextFormat,
    /// Settings panel text: labels on the left, values on the right.
    control_fmt: IDWriteTextFormat,
    value_fmt: IDWriteTextFormat,
    /// Icon bitmaps belong to `target`; replaced together with it.
    icons: IconCache,
}

fn text_format(
    dw: &IDWriteFactory,
    family: &str,
    weight: DWRITE_FONT_WEIGHT,
    size: f32,
) -> Result<IDWriteTextFormat> {
    unsafe {
        dw.CreateTextFormat(
            &HSTRING::from(family),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("en-us"),
        )
    }
}

fn color(c: Rgba) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.r,
        g: c.g,
        b: c.b,
        a: c.a,
    }
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

fn rounded(r: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: r,
        radiusX: radius,
        radiusY: radius,
    }
}

/// Gradient line from (0, y0) to (0, y1). windows 0.62 types the end points
/// as `windows_numerics::Vector2`, a crate this project does not depend on
/// directly; the struct is four `f32`s (x0, y0, x1, y1), and `transmute`
/// checks the size at compile time.
fn vertical_line(y0: f32, y1: f32) -> D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
    unsafe {
        std::mem::transmute::<[f32; 4], D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES>([0.0, y0, 0.0, y1])
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Rectangle of the title-bar glyph box `index` places from the right edge
/// (0 = the lock), in DIPs.
pub fn glyph_box(frame_w_dip: f32, index: usize) -> D2D_RECT_F {
    let right = frame_w_dip - theme::GLYPH_MARGIN - theme::GLYPH_BOX * index as f32;
    let top = (theme::TITLE_H - theme::GLYPH_BOX) / 2.0;
    rect(right - theme::GLYPH_BOX, top, right, top + theme::GLYPH_BOX)
}

impl Renderer {
    /// See `IconCache::wants_thumbnail`.
    pub fn wants_thumbnail(&mut self, key: &ThumbKey) -> bool {
        self.icons.wants_thumbnail(key)
    }

    /// See `IconCache::thumbnail_ready`.
    pub fn thumbnail_ready(&mut self, key: ThumbKey, image: Option<HBITMAP>) {
        self.icons.thumbnail_ready(key, image);
    }

    pub fn new() -> Result<Renderer> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let title_fmt = text_format(
                &dw,
                theme::FONT_TEXT,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                theme::TITLE_FONT_PX,
            )?;
            title_fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            title_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let ellipsis = dw.CreateEllipsisTrimmingSign(&title_fmt)?;
            let trimming = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            };
            title_fmt.SetTrimming(&trimming, &ellipsis)?;

            let message_fmt = text_format(
                &dw,
                theme::FONT_TEXT,
                DWRITE_FONT_WEIGHT_NORMAL,
                theme::LABEL_FONT_PX,
            )?;
            message_fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            message_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let glyph_fmt = text_format(
                &dw,
                theme::FONT_GLYPHS,
                DWRITE_FONT_WEIGHT_NORMAL,
                theme::GLYPH_FONT_PX,
            )?;
            glyph_fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            glyph_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            // Item labels: centred, up to two lines, long words broken,
            // ellipsis where the second line runs out.
            let label_fmt = text_format(
                &dw,
                theme::FONT_TEXT,
                DWRITE_FONT_WEIGHT_NORMAL,
                theme::LABEL_FONT_PX,
            )?;
            label_fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            label_fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
            label_fmt.SetLineSpacing(
                DWRITE_LINE_SPACING_METHOD_UNIFORM,
                theme::LABEL_LINE_H,
                theme::LABEL_LINE_H * 0.8,
            )?;
            let label_ellipsis = dw.CreateEllipsisTrimmingSign(&label_fmt)?;
            label_fmt.SetTrimming(&trimming, &label_ellipsis)?;

            let group_fmt = text_format(
                &dw,
                theme::FONT_TEXT,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                theme::GROUP_FONT_PX,
            )?;
            group_fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            group_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let group_ellipsis = dw.CreateEllipsisTrimmingSign(&group_fmt)?;
            group_fmt.SetTrimming(&trimming, &group_ellipsis)?;

            let control_fmt = text_format(
                &dw,
                theme::FONT_TEXT,
                DWRITE_FONT_WEIGHT_NORMAL,
                theme::CONTROL_FONT_PX,
            )?;
            control_fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            control_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let value_fmt = text_format(
                &dw,
                theme::FONT_TEXT,
                DWRITE_FONT_WEIGHT_NORMAL,
                theme::CONTROL_FONT_PX,
            )?;
            value_fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            value_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            value_fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;

            Ok(Renderer {
                d2d,
                dw,
                target: None,
                title_fmt,
                message_fmt,
                glyph_fmt,
                label_fmt,
                group_fmt,
                control_fmt,
                value_fmt,
                icons: IconCache::new(),
            })
        }
    }

    fn target(&mut self) -> Result<ID2D1DCRenderTarget> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        // Software rendering: frames are small and the result is copied into a
        // GDI DIB anyway. The hardware path loads the GPU driver into the
        // process (measured 55 MB private vs 5 MB; caveat 11).
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let t = unsafe { self.d2d.CreateDCRenderTarget(&props)? };
        unsafe { t.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE) };
        self.target = Some(t.clone());
        Ok(t)
    }

    /// Draws `scene` at `size_px`, `scale` px per DIP, and shows it in the
    /// layered window `hwnd`.
    pub fn draw_frame(
        &mut self,
        hwnd: HWND,
        surface: &mut Surface,
        size_px: (i32, i32),
        scale: f32,
        scene: &FrameScene,
    ) -> Result<()> {
        self.present(hwnd, surface, size_px, scale, |r, t, wd, hd| {
            r.paint(t, wd, hd, scale, scene)
        })
    }

    /// Draws a frame's settings panel and shows it in the layered window
    /// `hwnd`.
    pub fn draw_panel(
        &mut self,
        hwnd: HWND,
        surface: &mut Surface,
        size_px: (i32, i32),
        scale: f32,
        layout: &panel::Layout,
        scene: &panel::PanelScene,
    ) -> Result<()> {
        self.present(hwnd, surface, size_px, scale, |r, t, wd, hd| {
            r.paint_panel(t, wd, hd, layout, scene)
        })
    }

    /// Runs `paint` (in DIPs, `scale` px per DIP) into `surface` and pushes
    /// the result to the layered window `hwnd`.
    fn present(
        &mut self,
        hwnd: HWND,
        surface: &mut Surface,
        size_px: (i32, i32),
        scale: f32,
        paint: impl FnOnce(&mut Self, &ID2D1RenderTarget, f32, f32) -> Result<()>,
    ) -> Result<()> {
        let (w, h) = size_px;
        if w <= 0 || h <= 0 {
            return Ok(());
        }
        surface.ensure(w, h)?;
        let t = self.target()?;
        unsafe {
            t.BindDC(
                surface.dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
            )?;
            t.SetDpi(96.0 * scale, 96.0 * scale);
            t.BeginDraw();
            t.Clear(Some(&color(theme::rgb(0, 0.0))));
            let painted = paint(self, &t, w as f32 / scale, h as f32 / scale);
            let ended = t.EndDraw(None, None);
            if let Err(e) = &ended {
                if e.code() == D2DERR_RECREATE_TARGET {
                    self.target = None;
                    self.icons = IconCache::new();
                }
            }
            painted?;
            ended?;

            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&size),
                Some(surface.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }
        Ok(())
    }

    fn paint(
        &mut self,
        t: &ID2D1RenderTarget,
        wd: f32,
        hd: f32,
        scale: f32,
        scene: &FrameScene,
    ) -> Result<()> {
        let p = theme::palette(scene.light, scene.blurred, scene.opacity);
        unsafe {
            let brush = |c: Rgba| t.CreateSolidColorBrush(&color(c), None);
            let radius = scene.corner_radius;

            self.paint_glass(t, wd, hd, radius, &p)?;
            if scene.drop_frame {
                // Drop target: an accent outline around the whole frame.
                t.DrawRoundedRectangle(
                    &rounded(rect(1.0, 1.0, wd - 1.0, hd - 1.0), radius - 0.5),
                    &brush(theme::accent())?,
                    2.0,
                    None::<&ID2D1StrokeStyle>,
                );
            }

            // Lock (when locked), in the right corner, only while hovered.
            let mut title_right = wd - theme::TITLE_PAD_LEFT;
            if scene.hover && scene.locked {
                let lock = glyph_box(wd, 0);
                self.text(
                    t,
                    &theme::GLYPH_LOCK.to_string(),
                    &self.glyph_fmt,
                    lock,
                    &brush(p.dim)?,
                );
                title_right = lock.left;
            }

            let title_rect = rect(
                theme::TITLE_PAD_LEFT,
                0.0,
                title_right - 4.0,
                theme::TITLE_H,
            );
            self.text(
                t,
                &scene.title,
                &self.title_fmt,
                title_rect,
                &brush(p.title)?,
            );

            if !scene.collapsed {
                if let Some(msg) = &scene.message {
                    let r = rect(8.0, theme::TITLE_H, wd - 8.0, hd - 8.0);
                    self.text(t, msg, &self.message_fmt, r, &brush(p.dim)?);
                }
            }
        }
        if let Some(g) = &scene.grid {
            self.paint_grid(t, wd, hd, scale, g, &p)?;
        }
        Ok(())
    }

    /// Glass: vertical tint gradient, hairline border inset half a DIP so it
    /// is crisp, and a light catch along the top edge.
    fn paint_glass(
        &self,
        t: &ID2D1RenderTarget,
        wd: f32,
        hd: f32,
        radius: f32,
        p: &theme::Palette,
    ) -> Result<()> {
        let no_stroke: Option<&ID2D1StrokeStyle> = None;
        unsafe {
            let outer = rounded(rect(0.5, 0.5, wd - 0.5, hd - 0.5), radius);
            let fill = self.vertical_gradient(t, p.tint_top, p.tint_bottom, 0.0, hd)?;
            t.FillRoundedRectangle(&outer, &fill);
            let border = t.CreateSolidColorBrush(&color(p.border), None)?;
            t.DrawRoundedRectangle(&outer, &border, 1.0, no_stroke);
            let inner = rounded(rect(1.5, 1.5, wd - 1.5, hd - 1.5), radius - 1.0);
            let clear = Rgba {
                a: 0.0,
                ..p.highlight
            };
            let catch = self.vertical_gradient(t, p.highlight, clear, 1.0, radius.min(hd))?;
            t.DrawRoundedRectangle(&inner, &catch, 1.0, no_stroke);
        }
        Ok(())
    }

    /// The settings panel: glass, title, opacity slider, three switches and
    /// Remove frame; hover fills and a keyboard focus ring.
    fn paint_panel(
        &mut self,
        t: &ID2D1RenderTarget,
        wd: f32,
        hd: f32,
        l: &panel::Layout,
        sc: &panel::PanelScene,
    ) -> Result<()> {
        let p = theme::palette(sc.light, sc.blurred, Some(theme::PANEL_OPACITY));
        let accent = theme::accent();
        let no_stroke: Option<&ID2D1StrokeStyle> = None;
        self.paint_glass(t, wd, hd, theme::RADIUS, &p)?;
        unsafe {
            let brush = |c: Rgba| t.CreateSolidColorBrush(&color(c), None);
            let faded = |c: Rgba, on: bool| if on { c } else { Rgba { a: c.a * 0.4, ..c } };
            let circle =
                |cx: f32, cy: f32, r: f32| rounded(rect(cx - r, cy - r, cx + r, cy + r), r);
            let row_hover = |c: Control, enabled: bool| -> Result<()> {
                if enabled && sc.hover == Some(c) {
                    let r = l.rect(c);
                    t.FillRoundedRectangle(
                        &rounded(rect(r.x, r.y, r.x + r.w, r.y + r.h), theme::CELL_RADIUS),
                        &brush(p.glyph_hover)?,
                    );
                }
                Ok(())
            };

            let tr = l.title;
            self.text(
                t,
                "Frame settings",
                &self.title_fmt,
                rect(tr.x + 8.0, tr.y, tr.x + tr.w - 8.0, tr.y + tr.h),
                &brush(p.title)?,
            );

            // Opacity: label and value, then the slider.
            let lr = l.opacity_label;
            let text_rect = rect(lr.x + 12.0, lr.y, lr.x + lr.w - 12.0, lr.y + lr.h);
            self.text(t, "Opacity", &self.control_fmt, text_rect, &brush(p.title)?);
            let percent = format!("{} %", (sc.opacity * 100.0).round());
            self.text(t, &percent, &self.value_fmt, text_rect, &brush(p.dim)?);
            row_hover(Control::Opacity, true)?;
            let cy = l.slider.y + l.slider.h / 2.0;
            let x = l.slider_x(sc.opacity);
            t.FillRoundedRectangle(
                &rounded(rect(l.track_x0, cy - 2.0, l.track_x1, cy + 2.0), 2.0),
                &brush(Rgba { a: 0.35, ..p.dim })?,
            );
            t.FillRoundedRectangle(
                &rounded(rect(l.track_x0, cy - 2.0, x, cy + 2.0), 2.0),
                &brush(accent)?,
            );
            t.FillRoundedRectangle(&circle(x, cy, 9.0), &brush(p.knob)?);
            t.DrawRoundedRectangle(&circle(x, cy, 9.0), &brush(p.border)?, 1.0, no_stroke);
            let dot = if sc.hover == Some(Control::Opacity) {
                6.0
            } else {
                5.0
            };
            t.FillRoundedRectangle(&circle(x, cy, dot), &brush(accent)?);

            // Switches.
            let switches = [
                (Control::Blur, "Blur behind", sc.blur, true),
                (
                    Control::HideExtensions,
                    "Hide extensions",
                    sc.hide_extensions,
                    true,
                ),
                (
                    Control::Lock,
                    "Lock in place",
                    sc.locked || sc.lock_all,
                    !sc.lock_all,
                ),
            ];
            for (c, label, on, enabled) in switches {
                row_hover(c, enabled)?;
                let r = l.rect(c);
                let cy = r.y + r.h / 2.0;
                let text_rect = rect(r.x + 12.0, r.y, r.x + r.w - 64.0, r.y + r.h);
                self.text(
                    t,
                    label,
                    &self.control_fmt,
                    text_rect,
                    &brush(faded(p.title, enabled))?,
                );
                let right = r.x + r.w - 12.0;
                let pill = rounded(rect(right - 40.0, cy - 10.0, right, cy + 10.0), 10.0);
                if on {
                    t.FillRoundedRectangle(&pill, &brush(faded(accent, enabled))?);
                    t.FillRoundedRectangle(
                        &circle(right - 10.0, cy, 6.0),
                        &brush(theme::rgb(0xFFFFFF, 1.0))?,
                    );
                } else {
                    t.DrawRoundedRectangle(&pill, &brush(faded(p.dim, enabled))?, 1.0, no_stroke);
                    t.FillRoundedRectangle(
                        &circle(right - 30.0, cy, 5.0),
                        &brush(faded(p.dim, enabled))?,
                    );
                }
            }

            row_hover(Control::Remove, true)?;
            let rr = l.remove;
            self.text(
                t,
                "Remove frame",
                &self.control_fmt,
                rect(rr.x + 12.0, rr.y, rr.x + rr.w - 12.0, rr.y + rr.h),
                &brush(p.danger)?,
            );

            if let Some(f) = sc.focus {
                let r = l.rect(f);
                t.DrawRoundedRectangle(
                    &rounded(
                        rect(r.x + 1.0, r.y + 1.0, r.x + r.w - 1.0, r.y + r.h - 1.0),
                        theme::CELL_RADIUS,
                    ),
                    &brush(accent)?,
                    1.5,
                    no_stroke,
                );
            }
        }
        Ok(())
    }

    /// The visible cells (hover/selection fill, icon, label) and the
    /// scrollbar, clipped to the content area. `s` is the frame's drawing scale; cell
    /// positions come in physical px and are drawn in DIPs.
    fn paint_grid(
        &mut self,
        t: &ID2D1RenderTarget,
        wd: f32,
        hd: f32,
        s: f32,
        g: &GridScene,
        p: &theme::Palette,
    ) -> Result<()> {
        let top = theme::TITLE_H;
        let view_px = (hd - top) * s;
        let accent = theme::accent();
        let no_stroke: Option<&ID2D1StrokeStyle> = None;
        unsafe {
            let brush = |c: Rgba| t.CreateSolidColorBrush(&color(c), None);
            let hover = brush(p.cell_hover)?;
            let select_fill = brush(Rgba {
                a: theme::SELECT_FILL_ALPHA,
                ..accent
            })?;
            let select_ring = brush(Rgba {
                a: theme::SELECT_RING_ALPHA,
                ..accent
            })?;
            let label = brush(p.label)?;
            let group_text = brush(p.group_text)?;
            let group_rule = brush(p.group_rule)?;
            let icon_px = theme::icon_px(s) as f32;

            // Stop short of the bottom corners' curve.
            t.PushAxisAlignedClip(
                &rect(1.0, top, wd - 1.0, hd - 3.0),
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
            // Nothing in the clip may return early: the clip must be popped.
            self.paint_group_headers(t, top, s, view_px, g, (&group_text, &group_rule));
            for i in g.layout.visible(g.scroll, view_px) {
                let c = g.layout.cell_rect(i);
                let y = top * s + c.y - g.scroll;
                let cell = rect(c.x / s, y / s, (c.x + c.w) / s, (y + c.h) / s);
                if g.selected == Some(i) || g.drop_cell == Some(i) {
                    t.FillRoundedRectangle(&rounded(cell, theme::CELL_RADIUS), &select_fill);
                    let ring = rect(
                        cell.left + 0.5,
                        cell.top + 0.5,
                        cell.right - 0.5,
                        cell.bottom - 0.5,
                    );
                    t.DrawRoundedRectangle(
                        &rounded(ring, theme::CELL_RADIUS - 0.5),
                        &select_ring,
                        1.0,
                        no_stroke,
                    );
                } else if g.hover == Some(i) {
                    t.FillRoundedRectangle(&rounded(cell, theme::CELL_RADIUS), &hover);
                }

                // Icon on whole pixels so it stays sharp.
                let ix = (c.x + (c.w - icon_px) / 2.0).round();
                let iy = (y + theme::ICON_TOP * s).round();
                // Pictures and videos show their preview once it is made.
                let preview = ThumbKey::of(&g.entries[i], icon_px as u32)
                    .and_then(|key| self.icons.thumbnail(&key, t));
                if let Some(bitmap) = preview {
                    let size = bitmap.GetPixelSize();
                    let r = grid::thumb_rect(icon_px, size.width as f32, size.height as f32);
                    let (x0, y0) = ((ix + r.x).round(), (iy + r.y).round());
                    let (x1, y1) = ((ix + r.x + r.w).round(), (iy + r.y + r.h).round());
                    t.DrawBitmap(
                        &bitmap,
                        Some(&rect(x0 / s, y0 / s, x1 / s, y1 / s)),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                } else if let Some(bitmap) = g.icons[i]
                    .and_then(|key| self.icons.get(key, &g.entries[i].path, icon_px as u32, t))
                {
                    let dest = rect(ix / s, iy / s, (ix + icon_px) / s, (iy + icon_px) / s);
                    t.DrawBitmap(
                        &bitmap,
                        Some(&dest),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                }

                let label_top = (iy + icon_px) / s + theme::LABEL_GAP;
                let r = rect(
                    cell.left + theme::LABEL_PAD_X,
                    label_top,
                    cell.right - theme::LABEL_PAD_X,
                    label_top + theme::LABEL_LINE_H * theme::LABEL_LINES,
                );
                let e = &g.entries[i];
                let name = folder::display_name(&e.name, e.is_dir, g.hide_extensions);
                self.text(t, name, &self.label_fmt, r, &label);
            }
            t.PopAxisAlignedClip();

            let max = g.layout.max_scroll(view_px);
            if max > 0.0 {
                let track_top = top + theme::CONTENT_PAD_BOTTOM;
                let track = hd - theme::CONTENT_PAD_BOTTOM - track_top;
                let thumb = (track * view_px / g.layout.content_height())
                    .max(theme::SCROLLBAR_MIN_H)
                    .min(track);
                let y = track_top + (track - thumb) * g.scroll / max;
                let x = wd - theme::SCROLLBAR_RIGHT - theme::SCROLLBAR_W;
                t.FillRoundedRectangle(
                    &rounded(
                        rect(x, y, x + theme::SCROLLBAR_W, y + thumb),
                        theme::SCROLLBAR_W / 2.0,
                    ),
                    &brush(p.scrollbar)?,
                );
            }
        }
        Ok(())
    }

    /// The type-group headers in view: the title on the left, then a thin
    /// line to the strip's right end (left out if the title can't be measured).
    fn paint_group_headers(
        &self,
        t: &ID2D1RenderTarget,
        top: f32,
        s: f32,
        view_px: f32,
        g: &GridScene,
        (text, rule): (&ID2D1SolidColorBrush, &ID2D1SolidColorBrush),
    ) {
        unsafe {
            for gi in g.layout.visible_groups(g.scroll, view_px) {
                let (Some(h), Some(title)) = (g.layout.header_rect(gi), g.group_titles.get(gi))
                else {
                    continue;
                };
                let y = top * s + h.y - g.scroll;
                let r = rect(
                    h.x / s + theme::GROUP_TEXT_X,
                    y / s,
                    (h.x + h.w) / s,
                    (y + h.h) / s,
                );
                let room = (r.right - r.left).max(0.0);
                self.text(t, title, &self.group_fmt, r, text);
                let Some(width) = self.text_width(title, room, r.bottom - r.top) else {
                    continue;
                };

                // One physical pixel, on a whole pixel, through the middle.
                let line_x = r.left + width + theme::GROUP_RULE_GAP;
                if line_x < r.right {
                    let mid = (y + h.h / 2.0).round() / s;
                    t.FillRectangle(&rect(line_x, mid, r.right, mid + 1.0 / s), rule);
                }
            }
        }
    }

    /// Width of `title` in the group header format, trimmed to `room`.
    fn text_width(&self, title: &str, room: f32, height: f32) -> Option<f32> {
        unsafe {
            let layout = self
                .dw
                .CreateTextLayout(&wide(title), &self.group_fmt, room, height)
                .ok()?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m).ok()?;
            Some(m.width)
        }
    }

    /// Brush that fades from `top` at `y0` to `bottom` at `y1` (DIPs).
    fn vertical_gradient(
        &self,
        t: &ID2D1RenderTarget,
        top: Rgba,
        bottom: Rgba,
        y0: f32,
        y1: f32,
    ) -> Result<ID2D1LinearGradientBrush> {
        let stops = [
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: color(top),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: color(bottom),
            },
        ];
        unsafe {
            let coll =
                t.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
            t.CreateLinearGradientBrush(&vertical_line(y0, y1), None, &coll)
        }
    }

    fn text(
        &self,
        t: &ID2D1RenderTarget,
        s: &str,
        fmt: &IDWriteTextFormat,
        r: D2D_RECT_F,
        brush: &ID2D1SolidColorBrush,
    ) {
        unsafe {
            t.DrawText(
                &wide(s),
                fmt,
                &r,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_resize_releases_old_bitmap_once() {
        let mut s = Surface::new();
        s.ensure(10, 10).unwrap();
        s.ensure(20, 30).unwrap();
        assert_eq!((s.w, s.h), (20, 30));
        assert!(!s.dc.is_invalid());
    }
}

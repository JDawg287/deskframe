//! Design constants in DIPs (1/96 inch), from the spec and the frame mock
//! (`docs/design/mock`). Every size is multiplied by the frame's drawing
//! scale when drawn (DPI / 96, zoomed at another resolution).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows::core::{w, BOOL, HSTRING};
use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// Frame corner radius at the window's DPI. Windows 11's own rounded corners
/// (DWMWCP_ROUND) clip the blur behind a frame; their radius is fixed at
/// 8 DIP of the window's DPI, so a zoomed frame draws it as
/// `RADIUS × (dpi / 96) / scale` of its own units. A larger radius would show
/// blur in the corners (screenshot `docs/bleeding.png`, in git history at 2d82dac). Checked at 150 %.
pub const RADIUS: f32 = 8.0;
/// Header height, including the 1 px border; also the collapsed height.
pub const TITLE_H: f32 = 44.0;
pub const TITLE_PAD_LEFT: f32 = 16.0;
pub const TITLE_FONT_PX: f32 = 14.0;
/// Settings panel labels and values.
pub const CONTROL_FONT_PX: f32 = 13.0;
/// The settings panel's tint: solid enough to read over anything.
pub const PANEL_OPACITY: f32 = 0.85;
pub const LABEL_FONT_PX: f32 = 12.0;
/// Type-group header text ("Pictures (12)"), Semibold.
pub const GROUP_FONT_PX: f32 = 11.0;
/// Header text inset from the strip's left edge, and the gap before its rule.
pub const GROUP_TEXT_X: f32 = 4.0;
pub const GROUP_RULE_GAP: f32 = 8.0;
pub const GLYPH_FONT_PX: f32 = 16.0;
/// Square box for the title-bar lock glyph.
pub const GLYPH_BOX: f32 = 32.0;
/// Gap between the last glyph box and the frame's right edge.
pub const GLYPH_MARGIN: f32 = 6.0;
/// Tint strength over a blurred backdrop, and without one (blur unavailable
/// or transparency effects off), where the tint alone must keep text legible.
pub const TINT_ALPHA_BLUR: f32 = 0.45;
pub const TINT_ALPHA_SOLID: f32 = 0.85;
/// Range of a frame's own opacity setting (the settings panel's slider).
pub const OPACITY_MIN: f32 = 0.20;
pub const OPACITY_MAX: f32 = 1.0;
/// The top of the card is this much more opaque than the bottom.
pub const TINT_TOP_EXTRA: f32 = 0.08;
/// Icon grid (glass mock): cell size and content padding around the cells.
pub const CELL_W: f32 = 76.0;
pub const CELL_H: f32 = 84.0;
pub const CONTENT_PAD_X: f32 = 8.0;
pub const CONTENT_PAD_TOP: f32 = 2.0;
pub const CONTENT_PAD_BOTTOM: f32 = 8.0;
/// Height of a type-group header strip ("Pictures (12)").
pub const GROUP_HEADER_H: f32 = 24.0;
/// Inside a cell: hover/selection corner radius, the icon (centred, this far
/// from the cell top) and up to two label lines under it.
pub const CELL_RADIUS: f32 = 6.0;
pub const ICON_PX: f32 = 48.0;

/// The icon (and preview) square in physical px at drawing scale `s`.
pub fn icon_px(s: f32) -> u32 {
    (ICON_PX * s).round() as u32
}
pub const ICON_TOP: f32 = 3.0;
pub const LABEL_GAP: f32 = 2.0;
pub const LABEL_PAD_X: f32 = 2.0;
pub const LABEL_LINE_H: f32 = 15.0;
pub const LABEL_LINES: f32 = 2.0;
/// Selection: accent fill and a 1 px accent ring.
pub const SELECT_FILL_ALPHA: f32 = 0.24;
pub const SELECT_RING_ALPHA: f32 = 0.75;
/// Thin scrollbar on the right of the content, shown only on overflow.
pub const SCROLLBAR_W: f32 = 3.0;
pub const SCROLLBAR_RIGHT: f32 = 4.0;
pub const SCROLLBAR_MIN_H: f32 = 24.0;
/// Default expanded size of a new frame: 4 columns of 76 px plus padding,
/// header plus 2.5 rows of 84 px.
pub const DEFAULT_FRAME_W: f32 = 4.0 * CELL_W + 2.0 * CONTENT_PAD_X;
pub const DEFAULT_FRAME_H: f32 = TITLE_H + CONTENT_PAD_TOP + 2.5 * CELL_H + CONTENT_PAD_BOTTOM;

pub const FONT_TEXT: &str = "Segoe UI Variable Text";
pub const FONT_GLYPHS: &str = "Segoe Fluent Icons";
pub const GLYPH_LOCK: char = '\u{E72E}';

/// A straight (not premultiplied) colour, 0.0–1.0 per channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

pub fn rgb(hex: u32, a: f32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Glass tint at the top and bottom of the card (vertical gradient).
    pub tint_top: Rgba,
    pub tint_bottom: Rgba,
    pub border: Rgba,
    /// One-pixel light catch along the top edge, fading out by `RADIUS`.
    pub highlight: Rgba,
    pub title: Rgba,
    pub dim: Rgba,
    pub glyph_hover: Rgba,
    pub label: Rgba,
    pub cell_hover: Rgba,
    pub scrollbar: Rgba,
    /// "Remove frame" and other destructive actions.
    pub danger: Rgba,
    /// The slider's thumb.
    pub knob: Rgba,
    /// Type-group header text and the thin line after it.
    pub group_text: Rgba,
    pub group_rule: Rgba,
}

/// Colours from the glass mock (`docs/design/mock/Glass.dc.html`). The
/// tint strength is the frame's own `opacity` if set, else the default for
/// a blurred or a solid frame.
pub fn palette(light: bool, blurred: bool, opacity: Option<f32>) -> Palette {
    let a = match opacity {
        Some(o) => o.clamp(OPACITY_MIN, OPACITY_MAX),
        None if blurred => TINT_ALPHA_BLUR,
        None => TINT_ALPHA_SOLID,
    };
    let top = (a + TINT_TOP_EXTRA).min(1.0);
    if light {
        Palette {
            tint_top: rgb(0xFFFFFF, top),
            tint_bottom: rgb(0xF0F3F8, a),
            border: rgb(0xFFFFFF, 0.70),
            highlight: rgb(0xFFFFFF, 0.95),
            title: rgb(0x141414, 1.0),
            dim: rgb(0x000000, 0.62),
            glyph_hover: rgb(0x808080, 0.24),
            label: rgb(0x000000, 0.88),
            cell_hover: rgb(0xFFFFFF, 0.50),
            scrollbar: rgb(0x000000, 0.35),
            danger: rgb(0xC42B1C, 1.0),
            knob: rgb(0xFFFFFF, 1.0),
            group_text: rgb(0x000000, 0.60),
            group_rule: rgb(0x000000, 0.12),
        }
    } else {
        Palette {
            tint_top: rgb(0x3A3A42, top),
            tint_bottom: rgb(0x1C1C22, a),
            border: rgb(0xFFFFFF, 0.12),
            highlight: rgb(0xFFFFFF, 0.16),
            title: rgb(0xFFFFFF, 1.0),
            dim: rgb(0xFFFFFF, 0.70),
            glyph_hover: rgb(0x808080, 0.24),
            label: rgb(0xFFFFFF, 0.94),
            cell_hover: rgb(0xFFFFFF, 0.08),
            scrollbar: rgb(0xFFFFFF, 0.35),
            danger: rgb(0xFF99A4, 1.0),
            knob: rgb(0x454545, 1.0),
            group_text: rgb(0xFFFFFF, 0.62),
            group_rule: rgb(0xFFFFFF, 0.14),
        }
    }
}

static LIGHT: AtomicBool = AtomicBool::new(false);
/// DWM colorization value (0xAARRGGBB); Windows' default blue until
/// `refresh()` reads the real one.
static COLORIZATION: AtomicU32 = AtomicU32::new(0xFF00_78D4);

/// The cached app mode; `refresh()` updates it on `WM_SETTINGCHANGE`.
pub fn light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// The cached system accent colour (selection), opaque.
pub fn accent() -> Rgba {
    accent_from_colorization(COLORIZATION.load(Ordering::Relaxed))
}

/// Re-reads the app mode and accent colour. Returns true when either changed.
pub fn refresh() -> bool {
    let light = is_light();
    let light_changed = LIGHT.swap(light, Ordering::Relaxed) != light;
    let mut accent_changed = false;
    let (mut argb, mut opaque) = (0u32, BOOL(0));
    if unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.is_ok() {
        accent_changed = COLORIZATION.swap(argb, Ordering::Relaxed) != argb;
    }
    light_changed || accent_changed
}

/// Follows Settings > Personalization > Colors > "Choose your app mode".
fn is_light() -> bool {
    // Missing value means the Windows default, which is light.
    personalize_dword("AppsUseLightTheme").is_none_or(|v| v != 0)
}

/// Settings > Personalization > Colors > "Transparency effects". When off,
/// frames skip the blur and use the solid tint.
pub fn transparency_enabled() -> bool {
    personalize_dword("EnableTransparency").is_none_or(|v| v != 0)
}

fn personalize_dword(name: &str) -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            &HSTRING::from(name),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(value)
}

/// The accent colour from DWM's colorization value (0xAARRGGBB), opaque;
/// callers apply their own alpha.
fn accent_from_colorization(argb: u32) -> Rgba {
    rgb(argb & 0x00FF_FFFF, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_ignores_dwm_alpha() {
        assert_eq!(accent_from_colorization(0xC4_00_78_D4), rgb(0x0078D4, 1.0));
    }

    #[test]
    fn palette_uses_frame_opacity() {
        let p = palette(false, true, Some(0.7));
        assert_eq!(p.tint_bottom.a, 0.7);
        assert!((p.tint_top.a - (0.7 + TINT_TOP_EXTRA)).abs() < 1e-6);
        assert_eq!(palette(false, true, Some(0.98)).tint_top.a, 1.0);
    }

    #[test]
    fn opacity_is_clamped() {
        assert_eq!(palette(false, true, Some(0.05)).tint_bottom.a, OPACITY_MIN);
        assert_eq!(palette(false, true, Some(2.0)).tint_bottom.a, OPACITY_MAX);
    }

    #[test]
    fn no_opacity_uses_the_theme_default() {
        assert_eq!(palette(false, true, None).tint_bottom.a, TINT_ALPHA_BLUR);
        assert_eq!(palette(false, false, None).tint_bottom.a, TINT_ALPHA_SOLID);
    }
}

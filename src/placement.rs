//! Where a frame goes on screen: positions are saved per monitor (relative
//! to that monitor) and resolved against the monitors present now, so a
//! frame returns to its monitor when it comes back (caveat 4).

use windows::Win32::Foundation::RECT;

use crate::config::{FrameConfig, Placement};
use crate::display::MonitorInfo;
use crate::theme;

/// Where a frame goes and how large it draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    /// Screen px, expanded size.
    pub rect: RECT,
    /// Drawing scale (1.0 = 96 DPI sizes).
    pub scale: f32,
}

/// `abs` (screen px) as a position relative to `mon`'s top-left corner,
/// with the monitor's size and the frame's drawing `scale`.
pub fn to_relative(abs: RECT, mon: &MonitorInfo, scale: f32) -> Placement {
    Placement {
        x: abs.left - mon.rect.left,
        y: abs.top - mon.rect.top,
        w: abs.right - abs.left,
        h: abs.bottom - abs.top,
        mon_w: mon.rect.right - mon.rect.left,
        mon_h: mon.rect.bottom - mon.rect.top,
        scale,
    }
}

/// The monitor holding the centre of `abs`, else the nearest one.
pub fn monitor_for(abs: RECT, monitors: &[MonitorInfo]) -> Option<&MonitorInfo> {
    let (cx, cy) = ((abs.left + abs.right) / 2, (abs.top + abs.bottom) / 2);
    let distance = |m: &MonitorInfo| {
        let dx = (m.rect.left - cx).max(cx - (m.rect.right - 1)).max(0) as i64;
        let dy = (m.rect.top - cy).max(cy - (m.rect.bottom - 1)).max(0) as i64;
        dx * dx + dy * dy
    };
    monitors.iter().min_by_key(|m| distance(m))
}

/// Where `frame` goes now (screen px, expanded size): its spot on the
/// monitor it was last left on; else on another saved monitor that is
/// present; else on the primary monitor at the saved relative position (or
/// the centre for a new frame), zoomed if that monitor's resolution differs
/// from when it was saved. Always kept so the whole title bar is inside the
/// monitor's work area. `None` when no monitor is reported.
pub fn resolve(frame: &FrameConfig, monitors: &[MonitorInfo]) -> Option<Spot> {
    let present = |id: &str| monitors.iter().find(|m| m.device_id == id);
    let last = frame.last_monitor.as_deref();
    let saved = last
        .and_then(|id| Some((present(id)?, *frame.placements.get(id)?)))
        .or_else(|| {
            frame
                .placements
                .iter()
                .find_map(|(id, p)| Some((present(id)?, *p)))
        });
    let (mon, p) = match saved {
        Some(found) => found,
        None => {
            let primary = monitors.iter().find(|m| m.primary).or(monitors.first())?;
            let remembered = last
                .and_then(|id| frame.placements.get(id))
                .or(frame.placements.values().next());
            (
                primary,
                remembered.copied().unwrap_or_else(|| new_frame(primary)),
            )
        }
    };
    let (p, scale) = zoom(p, mon);
    Some(Spot {
        rect: keep_title_on_screen(mon, p, scale),
        scale,
    })
}

/// `p` brought to `mon`'s current size: position and size follow each
/// axis, the drawing scale follows the monitor's shorter side, so the frame
/// keeps its share of the screen and its contents their proportions (and a
/// switch there and back, even between aspect ratios, returns the same scale). A placement
/// without a saved monitor size (older config) is used as is, at the
/// monitor's DPI.
fn zoom(p: Placement, mon: &MonitorInfo) -> (Placement, f32) {
    if p.mon_w <= 0 || p.mon_h <= 0 || p.scale <= 0.0 {
        return (p, mon.dpi as f32 / 96.0);
    }
    let rx = (mon.rect.right - mon.rect.left) as f32 / p.mon_w as f32;
    let ry = (mon.rect.bottom - mon.rect.top) as f32 / p.mon_h as f32;
    let by = |v: i32, r: f32| (v as f32 * r).round() as i32;
    let zoomed = Placement {
        x: by(p.x, rx),
        y: by(p.y, ry),
        w: by(p.w, rx),
        h: by(p.h, ry),
        ..p
    };
    let short = |w: i32, h: i32| w.min(h) as f32;
    let now = short(
        mon.rect.right - mon.rect.left,
        mon.rect.bottom - mon.rect.top,
    );
    (zoomed, p.scale * now / short(p.mon_w, p.mon_h))
}

/// A new frame: default size, centred in the monitor's work area.
fn new_frame(mon: &MonitorInfo) -> Placement {
    let s = mon.dpi as f32 / 96.0;
    let (w, h) = (
        (theme::DEFAULT_FRAME_W * s).round() as i32,
        (theme::DEFAULT_FRAME_H * s).round() as i32,
    );
    let work = mon.work;
    Placement {
        x: work.left - mon.rect.left + (work.right - work.left - w) / 2,
        y: work.top - mon.rect.top + (work.bottom - work.top - h) / 2,
        w,
        h,
        ..Default::default()
    }
}

fn keep_title_on_screen(mon: &MonitorInfo, p: Placement, scale: f32) -> RECT {
    let work = mon.work;
    let title = (theme::TITLE_H * scale).round() as i32;
    let w = p.w.min(work.right - work.left);
    let left = (mon.rect.left + p.x).clamp(work.left, work.right - w);
    let top = (mon.rect.top + p.y).clamp(work.top, (work.bottom - title).max(work.top));
    RECT {
        left,
        top,
        right: left + w,
        bottom: top + p.h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    /// A monitor whose work area loses a 40 px taskbar at the bottom.
    fn monitor(
        id: &str,
        left: i32,
        top: i32,
        w: i32,
        h: i32,
        primary: bool,
        dpi: u32,
    ) -> MonitorInfo {
        MonitorInfo {
            device_id: id.to_string(),
            rect: rect(left, top, left + w, top + h),
            work: rect(left, top, left + w, top + h - 40),
            primary,
            dpi,
        }
    }

    fn frame(saved: &[(&str, Placement)], last: Option<&str>) -> FrameConfig {
        FrameConfig {
            placements: saved
                .iter()
                .map(|(id, p)| (id.to_string(), *p))
                .collect::<BTreeMap<_, _>>(),
            last_monitor: last.map(str::to_string),
            ..Default::default()
        }
    }

    /// A spot saved by an older config: no monitor size, no scale.
    const P: Placement = Placement {
        x: 80,
        y: 100,
        w: 480,
        h: 396,
        mon_w: 0,
        mon_h: 0,
        scale: 0.0,
    };

    #[test]
    fn relative_round_trip() {
        let mons = [monitor("A", 1920, 0, 1920, 1080, false, 96)];
        let abs = rect(2000, 100, 2480, 496);
        let rel = to_relative(abs, &mons[0], 1.0);
        assert_eq!(
            rel,
            Placement {
                mon_w: 1920,
                mon_h: 1080,
                scale: 1.0,
                ..P
            }
        );
        assert_eq!(
            resolve(&frame(&[("A", rel)], Some("A")), &mons)
                .unwrap()
                .rect,
            abs
        );
    }

    #[test]
    fn uses_last_monitor_when_present() {
        let mons = [
            monitor("A", 0, 0, 1920, 1080, true, 96),
            monitor("B", 1920, 0, 1920, 1080, false, 96),
        ];
        let other = Placement { x: 300, ..P };
        let f = frame(&[("A", other), ("B", P)], Some("B"));
        assert_eq!(resolve(&f, &mons).unwrap().rect, rect(2000, 100, 2480, 496));
    }

    #[test]
    fn uses_other_saved_monitor() {
        let mons = [monitor("A", 0, 0, 1920, 1080, true, 96)];
        let f = frame(&[("A", P), ("C", Placement { x: 900, ..P })], Some("C"));
        assert_eq!(resolve(&f, &mons).unwrap().rect, rect(80, 100, 560, 496));
    }

    #[test]
    fn missing_monitor_falls_back_to_primary_clamped() {
        let mons = [monitor("A", 0, 0, 1920, 1080, true, 96)];
        let on_4k = Placement {
            x: 3000,
            y: 2000,
            w: 480,
            h: 396,
            ..P
        };
        let r = resolve(&frame(&[("4K", on_4k)], Some("4K")), &mons)
            .unwrap()
            .rect;
        assert!(r.right <= 1920, "right edge {}", r.right);
        assert_eq!(r.right - r.left, 480);
        // The whole 44 px title bar stays above the taskbar (work bottom 1040).
        assert!(r.top + 44 <= 1040, "top {}", r.top);
    }

    #[test]
    fn negative_origin_monitor() {
        let mons = [
            monitor("L", -1920, 0, 1920, 1080, false, 96),
            monitor("A", 0, 0, 1920, 1080, true, 96),
        ];
        let r = resolve(
            &frame(&[("L", Placement { x: 100, ..P })], Some("L")),
            &mons,
        )
        .unwrap()
        .rect;
        assert_eq!((r.left, r.top), (-1820, 100));
    }

    #[test]
    fn new_frame_goes_to_primary_centre() {
        let mons = [
            monitor("B", -1920, 0, 1920, 1080, false, 96),
            monitor("A", 0, 0, 2560, 1440, true, 144),
        ];
        let r = resolve(&frame(&[], None), &mons).unwrap().rect;
        // Default size at 150 %, centred in the work area (2560 x 1400).
        assert_eq!((r.right - r.left, r.bottom - r.top), (480, 396));
        assert_eq!((r.left, r.top), ((2560 - 480) / 2, (1400 - 396) / 2));
    }

    #[test]
    fn no_monitors_resolves_to_nothing() {
        // Mid display change Windows can report no monitors; the frame must
        // stay where it is rather than shrink to nothing.
        assert_eq!(resolve(&frame(&[("A", P)], Some("A")), &[]), None);
    }

    #[test]
    fn frame_wider_than_monitor_starts_at_its_left_edge() {
        let mons = [monitor("A", 0, 0, 1280, 720, true, 96)];
        let wide = Placement {
            x: 500,
            y: 10,
            w: 2000,
            h: 300,
            ..P
        };
        let r = resolve(&frame(&[("A", wide)], Some("A")), &mons)
            .unwrap()
            .rect;
        assert_eq!((r.left, r.right), (0, 1280));
    }

    #[test]
    fn monitor_for_picks_the_one_holding_the_centre() {
        let mons = [
            monitor("A", 0, 0, 1920, 1080, true, 96),
            monitor("B", 1920, 0, 1920, 1080, false, 96),
        ];
        // Mostly on B: centre at x = 2000.
        let r = rect(1800, 100, 2200, 400);
        assert_eq!(monitor_for(r, &mons).unwrap().device_id, "B");
    }

    #[test]
    fn monitor_for_off_screen_picks_the_nearest() {
        let mons = [
            monitor("A", 0, 0, 1920, 1080, true, 96),
            monitor("B", 1920, 0, 1920, 1080, false, 96),
        ];
        let r = rect(4000, 100, 4400, 400);
        assert_eq!(monitor_for(r, &mons).unwrap().device_id, "B");
        assert!(monitor_for(r, &[]).is_none());
    }

    /// A spot saved on a `w`×`h` monitor at drawing scale `scale`.
    fn saved(x: i32, y: i32, w: i32, h: i32, mon: (i32, i32), scale: f32) -> Placement {
        Placement {
            x,
            y,
            w,
            h,
            mon_w: mon.0,
            mon_h: mon.1,
            scale,
        }
    }

    #[test]
    fn same_resolution_is_unchanged() {
        let mons = [monitor("A", 0, 0, 5120, 2160, true, 144)];
        let p = saved(600, 200, 480, 396, (5120, 2160), 1.5);
        let spot = resolve(&frame(&[("A", p)], Some("A")), &mons).unwrap();
        assert_eq!(spot.rect, rect(600, 200, 1080, 596));
        assert_eq!(spot.scale, 1.5);
    }

    #[test]
    fn half_resolution_halves_position_and_size() {
        // 5120x2160 at 150 % -> 2560x1080 at 100 %: same share of the screen,
        // contents drawn at half the saved scale.
        let mons = [monitor("A", 0, 0, 2560, 1080, true, 96)];
        let p = saved(600, 200, 480, 396, (5120, 2160), 1.5);
        let spot = resolve(&frame(&[("A", p)], Some("A")), &mons).unwrap();
        assert_eq!(spot.rect, rect(300, 100, 540, 298));
        assert_eq!(spot.scale, 0.75);
    }

    #[test]
    fn old_config_without_monitor_size_is_unchanged() {
        let mons = [monitor("A", 0, 0, 2560, 1080, true, 120)];
        let spot = resolve(&frame(&[("A", P)], Some("A")), &mons).unwrap();
        assert_eq!(spot.rect, rect(80, 100, 560, 496));
        assert_eq!(spot.scale, 1.25);
    }

    #[test]
    fn fallback_monitor_scales_from_saved_monitor() {
        // Saved on a 3840x2160 monitor that is gone; the 1920x1080 primary
        // shows the frame at the same share of the screen.
        let mons = [monitor("A", 0, 0, 1920, 1080, true, 96)];
        let p = saved(1920, 400, 480, 400, (3840, 2160), 1.0);
        let spot = resolve(&frame(&[("4K", p)], Some("4K")), &mons).unwrap();
        assert_eq!(spot.rect, rect(960, 200, 1200, 400));
        assert_eq!(spot.scale, 0.5);
    }

    #[test]
    fn contents_follow_the_shorter_side() {
        // Width unchanged, height halved: position and size follow each
        // axis, contents follow the shorter side (here the height).
        let mons = [monitor("A", 0, 0, 1920, 540, true, 96)];
        let p = saved(100, 200, 400, 300, (1920, 1080), 1.0);
        let spot = resolve(&frame(&[("A", p)], Some("A")), &mons).unwrap();
        assert_eq!(spot.rect, rect(100, 100, 500, 250));
        assert_eq!(spot.scale, 0.5);
    }

    #[test]
    fn aspect_change_round_trip_keeps_scale() {
        // 21:9 -> 16:9, the user drags the frame there (saving the zoomed
        // scale), then back to 21:9: contents return to their first size.
        let wide = [monitor("A", 0, 0, 5120, 2160, true, 144)];
        let hd = [monitor("A", 0, 0, 1920, 1080, true, 96)];
        let p = saved(600, 200, 480, 396, (5120, 2160), 1.5);
        let at_hd = resolve(&frame(&[("A", p)], Some("A")), &hd).unwrap();
        let dragged = to_relative(at_hd.rect, &hd[0], at_hd.scale);
        let back = resolve(&frame(&[("A", dragged)], Some("A")), &wide).unwrap();
        assert_eq!(back.scale, 1.5);
    }
}

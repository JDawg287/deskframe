//! Icon-grid layout for a frame's content area: which cell is where, which
//! cell a point hits, and how far the content scrolls. Pure math in physical
//! px, with the origin at the top-left of the content area (below the header).

use std::ops::Range;

use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One group of cells: an optional header strip, then its rows.
#[derive(Clone, Copy, Debug)]
struct Section {
    /// Index of its first item.
    first: usize,
    count: usize,
    /// Top of its header (or of its first row when there is no header).
    top: f32,
    /// Top of its first row.
    rows_y: f32,
    /// Bottom of its last row.
    end: f32,
}

#[derive(Clone, Debug)]
pub struct Layout {
    columns: usize,
    cell_w: f32,
    cell_h: f32,
    pad_x: f32,
    pad_top: f32,
    pad_bottom: f32,
    /// Width and height of a header strip; no headers when `headers` is off.
    header_w: f32,
    header_h: f32,
    headers: bool,
    /// In display order; `top`, `rows_y`, `end` and `first` all ascend.
    sections: Vec<Section>,
}

impl Layout {
    /// Lays out `item_count` cells in a content area `client_w` px wide.
    pub fn new(client_w: f32, item_count: usize, scale: f32) -> Layout {
        Layout::build(client_w, &[item_count], false, scale)
    }

    /// Lays out groups of `sizes` items, each under a header strip and
    /// starting on a new row. Items are numbered across all groups in order.
    pub fn grouped(client_w: f32, sizes: &[usize], scale: f32) -> Layout {
        Layout::build(client_w, sizes, true, scale)
    }

    fn build(client_w: f32, sizes: &[usize], headers: bool, scale: f32) -> Layout {
        // No groups: one empty section without a header, like `new` with no items.
        let (sizes, headers) = match sizes {
            [] => (&[0][..], false),
            _ => (sizes, headers),
        };
        let cell_w = theme::CELL_W * scale;
        let cell_h = theme::CELL_H * scale;
        let pad_x = theme::CONTENT_PAD_X * scale;
        let fit = ((client_w - 2.0 * pad_x) / cell_w).floor();
        let columns = if fit >= 1.0 { fit as usize } else { 1 };
        let header_h = if headers {
            theme::GROUP_HEADER_H * scale
        } else {
            0.0
        };
        let pad_top = theme::CONTENT_PAD_TOP * scale;
        let (mut y, mut first) = (pad_top, 0);
        let sections = sizes
            .iter()
            .map(|&count| {
                let rows_y = y + header_h;
                let end = rows_y + count.div_ceil(columns) as f32 * cell_h;
                let section = Section {
                    first,
                    count,
                    top: y,
                    rows_y,
                    end,
                };
                (y, first) = (end, first + count);
                section
            })
            .collect();
        Layout {
            columns,
            cell_w,
            cell_h,
            pad_x,
            pad_top,
            pad_bottom: theme::CONTENT_PAD_BOTTOM * scale,
            header_w: (client_w - 2.0 * pad_x).max(0.0),
            header_h,
            headers,
            sections,
        }
    }

    /// The section holding the content row at `y`: the last one starting
    /// at or above it.
    fn section_at(&self, y: f32) -> Option<&Section> {
        let n = self.sections.partition_point(|s| s.top <= y);
        n.checked_sub(1).map(|i| &self.sections[i])
    }

    /// Cell `i` in content coordinates (before scrolling).
    pub fn cell_rect(&self, i: usize) -> RectF {
        let n = self.sections.partition_point(|s| s.first <= i);
        let s = &self.sections[n.saturating_sub(1)];
        let local = i - s.first;
        let (row, col) = (local / self.columns, local % self.columns);
        RectF {
            x: self.pad_x + col as f32 * self.cell_w,
            y: s.rows_y + row as f32 * self.cell_h,
            w: self.cell_w,
            h: self.cell_h,
        }
    }

    pub fn content_height(&self) -> f32 {
        self.sections.last().map_or(self.pad_top, |s| s.end) + self.pad_bottom
    }

    /// The item under (`x`, `y`) in view coordinates, with the content
    /// scrolled down by `scroll_y`. `None` over padding, headers or empty cells.
    pub fn hit_test(&self, x: f32, y: f32, scroll_y: f32) -> Option<usize> {
        let cx = x - self.pad_x;
        let cy = y + scroll_y;
        if cx < 0.0 {
            return None;
        }
        let col = (cx / self.cell_w) as usize;
        if col >= self.columns {
            return None;
        }
        let s = self.section_at(cy)?;
        if cy < s.rows_y {
            return None;
        }
        let local = ((cy - s.rows_y) / self.cell_h) as usize * self.columns + col;
        (local < s.count).then_some(s.first + local)
    }

    /// Items at least partly inside a view `view_h` px tall scrolled to `scroll_y`.
    pub fn visible(&self, scroll_y: f32, view_h: f32) -> Range<usize> {
        let first_in = |s: &Section, y: f32, round: fn(f32) -> f32| {
            let rows = round(((y - s.rows_y) / self.cell_h).max(0.0)) as usize;
            s.first + (rows * self.columns).min(s.count)
        };
        let start = match self.section_at(scroll_y) {
            Some(s) => first_in(s, scroll_y, f32::floor),
            None => 0,
        };
        let end_y = scroll_y + view_h;
        let n = self.sections.partition_point(|s| s.top < end_y);
        let end = match n.checked_sub(1).map(|i| &self.sections[i]) {
            Some(s) => first_in(s, end_y, f32::ceil),
            None => 0,
        };
        start..end.max(start)
    }

    /// `scroll_y` moved by `rows` rows (negative = up), kept in range.
    pub fn scroll_by(&self, scroll_y: f32, rows: f32, view_h: f32) -> f32 {
        (scroll_y + rows * self.cell_h).clamp(0.0, self.max_scroll(view_h))
    }

    /// How far the content scrolls in a view `view_h` px tall.
    pub fn max_scroll(&self, view_h: f32) -> f32 {
        (self.content_height() - view_h).max(0.0)
    }

    /// Group `group`'s header strip in content coordinates; `None` when
    /// this layout has no headers.
    pub fn header_rect(&self, group: usize) -> Option<RectF> {
        let s = self.sections.get(group).filter(|_| self.headers)?;
        Some(RectF {
            x: self.pad_x,
            y: s.top,
            w: self.header_w,
            h: self.header_h,
        })
    }

    /// Groups whose header or items are at least partly inside the view.
    pub fn visible_groups(&self, scroll_y: f32, view_h: f32) -> Range<usize> {
        if !self.headers {
            return 0..0;
        }
        let start = self.sections.partition_point(|s| s.end <= scroll_y);
        let end = self.sections.partition_point(|s| s.top < scroll_y + view_h);
        start..end.max(start)
    }
}

/// Where a `w`×`h` preview goes in a `side` px icon square: shape kept,
/// never enlarged, centred across and resting on the bottom edge (as in
/// Explorer's icon views). Relative to the square's top-left.
pub fn thumb_rect(side: f32, w: f32, h: f32) -> RectF {
    let k = (side / w.max(1.0)).min(side / h.max(1.0)).min(1.0);
    let (w, h) = (w * k, h * k);
    RectF {
        x: (side - w) / 2.0,
        y: side - h,
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Glass mock: cells 76x84, content padding 2 top, 8 left/right/bottom.

    #[test]
    fn columns_fit_width() {
        // (400 - 2*8) / 76 = 5 columns: item 4 ends row 0, item 5 starts row 1.
        let l = Layout::new(400.0, 10, 1.0);
        assert_eq!(l.cell_rect(4).y, 2.0);
        assert_eq!(l.cell_rect(5).x, 8.0);
    }

    #[test]
    fn at_least_one_column_when_narrow() {
        let l = Layout::new(40.0, 3, 1.0);
        assert_eq!(l.cell_rect(1).y, 2.0 + 84.0);
    }

    #[test]
    fn cell_rect_wraps_rows() {
        let r = Layout::new(400.0, 10, 1.0).cell_rect(5);
        assert_eq!(
            r,
            RectF {
                x: 8.0,
                y: 2.0 + 84.0,
                w: 76.0,
                h: 84.0
            }
        );
    }

    #[test]
    fn hit_test_padding_is_none() {
        let l = Layout::new(400.0, 10, 1.0);
        assert_eq!(l.hit_test(4.0, 40.0, 0.0), None); // left padding
        assert_eq!(l.hit_test(40.0, 1.0, 0.0), None); // top padding
        assert_eq!(l.hit_test(390.0, 40.0, 0.0), None); // right of the last column
        assert_eq!(l.hit_test(40.0, 40.0, 0.0), Some(0));
    }

    #[test]
    fn hit_test_past_last_item_is_none() {
        let l = Layout::new(400.0, 7, 1.0);
        // Row 1 holds items 5 and 6; column 2 of row 1 is empty.
        assert_eq!(
            l.hit_test(8.0 + 76.0 + 10.0, 2.0 + 84.0 + 10.0, 0.0),
            Some(6)
        );
        assert_eq!(
            l.hit_test(8.0 + 2.0 * 76.0 + 10.0, 2.0 + 84.0 + 10.0, 0.0),
            None
        );
    }

    #[test]
    fn hit_test_respects_scroll() {
        let l = Layout::new(400.0, 10, 1.0);
        assert_eq!(l.hit_test(10.0, 12.0, 84.0), Some(5));
    }

    #[test]
    fn content_height_counts_partial_rows() {
        let l = Layout::new(400.0, 6, 1.0);
        assert_eq!(l.content_height(), 2.0 + 2.0 * 84.0 + 8.0);
        assert_eq!(Layout::new(400.0, 0, 1.0).content_height(), 2.0 + 8.0);
    }

    #[test]
    fn max_scroll_zero_when_fits() {
        assert_eq!(Layout::new(400.0, 3, 1.0).max_scroll(200.0), 0.0);
    }

    #[test]
    fn max_scroll_is_overflow() {
        let l = Layout::new(400.0, 10, 1.0);
        assert_eq!(l.max_scroll(100.0), l.content_height() - 100.0);
    }

    #[test]
    fn scale_150_scales_cells() {
        let r = Layout::new(600.0, 1, 1.5).cell_rect(0);
        assert_eq!((r.w, r.h), (114.0, 126.0));
        assert_eq!((r.x, r.y), (12.0, 3.0));
    }

    #[test]
    fn visible_at_top_is_first_rows() {
        // 5 columns; a 200 px view shows rows 0..=2 (row 2 starts at 170).
        let l = Layout::new(400.0, 100, 1.0);
        assert_eq!(l.visible(0.0, 200.0), 0..15);
    }

    #[test]
    fn visible_scrolled_skips_rows_above() {
        let l = Layout::new(400.0, 100, 1.0);
        // Scrolled past rows 0 and 1 (they end at 2 + 168 = 170).
        assert_eq!(l.visible(171.0, 84.0), 10..20);
    }

    #[test]
    fn visible_stops_at_item_count() {
        let l = Layout::new(400.0, 7, 1.0);
        assert_eq!(l.visible(0.0, 1_000.0), 0..7);
        assert_eq!(Layout::new(400.0, 0, 1.0).visible(0.0, 100.0), 0..0);
    }

    #[test]
    fn scroll_by_moves_whole_rows() {
        let l = Layout::new(400.0, 100, 1.0);
        assert_eq!(l.scroll_by(0.0, 1.0, 200.0), 84.0);
        assert_eq!(l.scroll_by(168.0, -1.0, 200.0), 84.0);
    }

    #[test]
    fn scroll_by_clamps_at_both_ends() {
        let l = Layout::new(400.0, 100, 1.0);
        assert_eq!(l.scroll_by(40.0, -3.0, 200.0), 0.0);
        assert_eq!(l.scroll_by(0.0, 1_000.0, 200.0), l.max_scroll(200.0));
    }

    // Grouped: header strips 24 high above each group.

    #[test]
    fn grouped_starts_each_group_on_a_new_row() {
        let l = Layout::grouped(400.0, &[3, 2], 1.0);
        assert_eq!(l.header_rect(0).unwrap().y, 2.0);
        assert_eq!(l.cell_rect(0).y, 26.0);
        assert_eq!(l.header_rect(1).unwrap().y, 110.0);
        let c = l.cell_rect(3);
        assert_eq!((c.x, c.y), (8.0, 134.0));
        assert_eq!(l.cell_rect(4).x, 8.0 + 76.0);
    }

    #[test]
    fn grouped_content_height_counts_headers() {
        let l = Layout::grouped(400.0, &[3, 2], 1.0);
        assert_eq!(l.content_height(), 2.0 + 24.0 + 84.0 + 24.0 + 84.0 + 8.0);
        assert_eq!(l.max_scroll(100.0), l.content_height() - 100.0);
    }

    #[test]
    fn hit_test_on_a_header_is_none() {
        let l = Layout::grouped(400.0, &[3, 2], 1.0);
        assert_eq!(l.hit_test(40.0, 5.0, 0.0), None);
        assert_eq!(l.hit_test(40.0, 115.0, 0.0), None);
        assert_eq!(l.hit_test(40.0, 30.0, 0.0), Some(0));
        assert_eq!(l.hit_test(10.0, 140.0, 0.0), Some(3));
        assert_eq!(l.hit_test(10.0, 40.0, 100.0), Some(3)); // scrolled
    }

    #[test]
    fn hit_test_past_a_groups_last_item_is_none() {
        let l = Layout::grouped(400.0, &[3, 2], 1.0);
        assert_eq!(l.hit_test(8.0 + 3.0 * 76.0 + 10.0, 40.0, 0.0), None);
        assert_eq!(l.hit_test(40.0, 300.0, 0.0), None); // below everything
    }

    #[test]
    fn grouped_visible_spans_groups() {
        // [7, 7]: group 0 rows at 26 and 110; header 1 at 194; rows at 218, 302.
        let l = Layout::grouped(400.0, &[7, 7], 1.0);
        assert_eq!(l.visible(150.0, 100.0), 5..12);
        assert_eq!(l.visible(0.0, 1_000.0), 0..14);
        assert_eq!(l.visible_groups(150.0, 100.0), 0..2);
        assert_eq!(l.visible_groups(250.0, 100.0), 1..2);
    }

    #[test]
    fn visible_groups_keeps_a_header_whose_items_are_off() {
        // The view ends inside header 1, above its first row.
        let l = Layout::grouped(400.0, &[7, 7], 1.0);
        assert_eq!(l.visible(0.0, 200.0), 0..7);
        assert_eq!(l.visible_groups(0.0, 200.0), 0..2);
    }

    #[test]
    fn header_rect_spans_content_width() {
        let l = Layout::grouped(400.0, &[3, 2], 1.0);
        assert_eq!(
            l.header_rect(0),
            Some(RectF {
                x: 8.0,
                y: 2.0,
                w: 384.0,
                h: 24.0
            })
        );
        assert_eq!(l.header_rect(2), None);
        assert_eq!(Layout::new(400.0, 10, 1.0).header_rect(0), None);
        assert_eq!(Layout::new(400.0, 10, 1.0).visible_groups(0.0, 100.0), 0..0);
    }

    #[test]
    fn grouped_with_no_groups_is_empty() {
        let l = Layout::grouped(400.0, &[], 1.0);
        assert_eq!(l.content_height(), 2.0 + 8.0);
        assert_eq!(l.visible(0.0, 100.0), 0..0);
        assert_eq!(l.visible_groups(0.0, 100.0), 0..0);
        assert_eq!(l.hit_test(40.0, 40.0, 0.0), None);
        assert_eq!(l.header_rect(0), None);
        assert_eq!(l.cell_rect(0).y, 2.0); // no panic
    }

    #[test]
    fn grouped_narrow_frame() {
        let l = Layout::grouped(40.0, &[2, 1], 1.0);
        assert_eq!(l.header_rect(0).unwrap().w, 24.0);
        assert_eq!(l.cell_rect(1).y, 2.0 + 24.0 + 84.0);
        assert_eq!(l.header_rect(1).unwrap().y, 2.0 + 24.0 + 168.0);
    }

    #[test]
    fn grouped_scales() {
        let l = Layout::grouped(600.0, &[1, 1], 1.5);
        assert_eq!(l.header_rect(0).unwrap().h, 36.0);
        assert_eq!(l.cell_rect(1).y, 3.0 + 36.0 + 126.0 + 36.0);
    }

    #[test]
    fn grouped_ten_thousand_items() {
        let l = Layout::grouped(400.0, &[100; 100], 1.0);
        assert!(l.content_height().is_finite());
        assert_eq!(l.cell_rect(100).y, 2.0 + 24.0 + 20.0 * 84.0 + 24.0);
        let c = l.cell_rect(9_999);
        assert_eq!(l.hit_test(c.x + 10.0, c.h / 2.0, c.y), Some(9_999));
        assert_eq!(l.visible(c.y, 10.0), 9_995..10_000);
    }

    #[test]
    fn ten_thousand_items_layout() {
        let l = Layout::new(400.0, 10_000, 1.0);
        assert!(l.content_height().is_finite());
        assert_eq!(l.cell_rect(9_999).y, 2.0 + 1_999.0 * 84.0);
    }

    #[test]
    fn thumb_rect_keeps_shape() {
        let r = |x, y, w, h| RectF { x, y, w, h };
        assert_eq!(thumb_rect(48.0, 96.0, 48.0), r(0.0, 24.0, 48.0, 24.0));
        assert_eq!(thumb_rect(48.0, 24.0, 48.0), r(12.0, 0.0, 24.0, 48.0));
        assert_eq!(thumb_rect(48.0, 20.0, 10.0), r(14.0, 38.0, 20.0, 10.0));
        assert_eq!(thumb_rect(48.0, 48.0, 48.0), r(0.0, 0.0, 48.0, 48.0));
    }
}

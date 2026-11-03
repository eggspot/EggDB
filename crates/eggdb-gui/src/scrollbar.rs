use crate::canvas::Rect;
use crate::color::Rgba;
use crate::main_window::Painter;

/// A track's thickness, in logical pixels.
pub const TRACK: f32 = 10.0;
const MIN_THUMB: f32 = 24.0;
const THUMB_ALPHA: f32 = 0.6;

/// Which way a scrollbar runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Up and down, at the region's right edge.
    Vertical,
    /// Left and right, at the region's bottom edge.
    Horizontal,
}

/// Where a region's content stands in its viewport along one axis, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extent {
    /// How much of the content shows.
    pub viewport: f32,
    /// How long the content is.
    pub content: f32,
    /// How far it is scrolled from its start.
    pub offset: f32,
}

impl Extent {
    /// Whether the content is longer than the viewport, so a scrollbar shows.
    pub fn scrolls(self) -> bool {
        self.content > self.viewport
    }

    fn range(self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    /// The offset as a percentage of how far it can go, 0 to 100, for screen readers.
    pub fn percent(self) -> u8 {
        if self.range() <= 0.0 {
            return 0;
        }
        (self.offset / self.range() * 100.0).round().clamp(0.0, 100.0) as u8
    }
}

/// A thumb's start along its track, from the track's start, and its length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thumb {
    /// Where it starts.
    pub start: f32,
    /// How long it is.
    pub length: f32,
}

/// The thumb on a track `track` long for `extent` (`docs/ux/workspace.md` 9.6): at least 24
/// long, `round(L × V / T)`, at `round((L − thumb) × O / (T − V))`; `None` when the content
/// fits.
pub fn thumb(track: f32, extent: Extent) -> Option<Thumb> {
    if !extent.scrolls() || track <= 0.0 {
        return None;
    }
    let length = (track * extent.viewport / extent.content)
        .round()
        .max(MIN_THUMB)
        .min(track);
    let offset = extent.offset.clamp(0.0, extent.range());
    let start = ((track - length) * offset / extent.range()).round();
    Some(Thumb { start, length })
}

/// The offset a thumb dragged to `pointer` sets, grabbed `grab` from its start, on a track
/// starting at `track_start` and `track` long: `(pointer − grab − trackStart) / (L − thumb) ×
/// (T − V)`, clamped.
pub fn dragged_offset(pointer: f32, grab: f32, track_start: f32, track: f32, extent: Extent) -> f32 {
    let Some(thumb) = thumb(track, extent) else {
        return 0.0;
    };
    let room = track - thumb.length;
    if room <= 0.0 {
        return 0.0;
    }
    ((pointer - grab - track_start) / room * extent.range()).clamp(0.0, extent.range())
}

/// The offset after a press on the track at `pointer` (from the track's start): one viewport
/// toward the pointer, clamped; unchanged on the thumb itself.
pub fn paged_offset(pointer: f32, track: f32, extent: Extent) -> f32 {
    let Some(thumb) = thumb(track, extent) else {
        return extent.offset;
    };
    let offset = if pointer < thumb.start {
        extent.offset - extent.viewport
    } else if pointer >= thumb.start + thumb.length {
        extent.offset + extent.viewport
    } else {
        extent.offset
    };
    offset.clamp(0.0, extent.range())
}

/// The track's rect along `axis` at a region's edge, the region ending at `right` and `bottom`.
pub fn track_rect(axis: Axis, start: f32, length: f32, right: f32, bottom: f32) -> Rect {
    match axis {
        Axis::Vertical => Rect::new(right - TRACK, start, TRACK, length),
        Axis::Horizontal => Rect::new(start, bottom - TRACK, length, TRACK),
    }
}

/// The thumb's rect in `track`: 6 wide inset 2, or 8 wide inset 1 while `hot`.
pub fn thumb_rect(axis: Axis, track: Rect, thumb: Thumb, hot: bool) -> Rect {
    let (inset, width) = if hot { (1.0, 8.0) } else { (2.0, 6.0) };
    match axis {
        Axis::Vertical => Rect::new(track.x + inset, track.y + thumb.start, width, thumb.length),
        Axis::Horizontal => Rect::new(track.x + thumb.start, track.y + inset, thumb.length, width),
    }
}

fn faded(color: Rgba, alpha: f32) -> Rgba {
    Rgba {
        a: (f32::from(color.a) * alpha).round() as u8,
        ..color
    }
}

/// Draws the scrollbar for `extent` in `track` along `axis`, nothing when the content fits:
/// the thumb `textDisabled` at 60 %, or `textSecondary` while `hot` (hovered or dragged).
pub(crate) fn draw(paint: &mut Painter<'_, '_>, axis: Axis, track: Rect, extent: Extent, hot: bool) {
    let length = match axis {
        Axis::Vertical => track.h,
        Axis::Horizontal => track.w,
    };
    let Some(thumb) = thumb(length, extent) else {
        return;
    };
    let rect = thumb_rect(axis, track, thumb, hot);
    let color = if hot {
        paint.theme.text_secondary
    } else {
        faded(paint.theme.text_disabled, THUMB_ALPHA)
    };
    let radius = rect.w.min(rect.h) / 2.0;
    paint.canvas.fill_rounded_rect(rect, radius, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(viewport: f32, content: f32, offset: f32) -> Extent {
        Extent {
            viewport,
            content,
            offset,
        }
    }

    #[test]
    fn content_that_fits_has_no_thumb_and_longer_content_has_one_in_proportion() {
        assert_eq!(thumb(330.0, extent(330.0, 330.0, 0.0)), None);
        assert_eq!(thumb(330.0, extent(330.0, 200.0, 0.0)), None);
        assert_eq!(
            thumb(330.0, extent(330.0, 660.0, 0.0)),
            Some(Thumb {
                start: 0.0,
                length: 165.0
            })
        );
        assert_eq!(
            thumb(330.0, extent(330.0, 660.0, 330.0)),
            Some(Thumb {
                start: 165.0,
                length: 165.0
            })
        );
        assert_eq!(
            thumb(330.0, extent(330.0, 24_000.0, 0.0)).map(|thumb| thumb.length),
            Some(24.0)
        );
        assert_eq!(extent(330.0, 660.0, 165.0).percent(), 50);
    }

    #[test]
    fn a_drag_and_a_track_press_set_the_offset_within_its_range() {
        let at = extent(330.0, 660.0, 0.0);
        assert_eq!(dragged_offset(446.0 + 82.5, 0.0, 446.0, 330.0, at), 165.0);
        assert_eq!(dragged_offset(2_000.0, 0.0, 446.0, 330.0, at), 330.0);
        assert_eq!(dragged_offset(-50.0, 0.0, 446.0, 330.0, at), 0.0);
        assert_eq!(paged_offset(300.0, 330.0, at), 330.0);
        assert_eq!(paged_offset(10.0, 330.0, at), 0.0);
        assert_eq!(paged_offset(10.0, 330.0, extent(330.0, 660.0, 330.0)), 0.0);
    }

    #[test]
    fn the_thumb_widens_while_hot() {
        let track = track_rect(Axis::Vertical, 446.0, 330.0, 1280.0, 776.0);
        assert_eq!(track, Rect::new(1270.0, 446.0, 10.0, 330.0));
        let thumb = Thumb {
            start: 10.0,
            length: 40.0,
        };
        assert_eq!(
            thumb_rect(Axis::Vertical, track, thumb, false),
            Rect::new(1272.0, 456.0, 6.0, 40.0)
        );
        assert_eq!(
            thumb_rect(Axis::Vertical, track, thumb, true),
            Rect::new(1271.0, 456.0, 8.0, 40.0)
        );
        let bottom = track_rect(Axis::Horizontal, 308.0, 962.0, 1280.0, 776.0);
        assert_eq!(bottom, Rect::new(308.0, 766.0, 962.0, 10.0));
    }
}

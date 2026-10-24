//! Putting the window back where it was.
//!
//! The Swift app saved the window's frame on every move and resize, and on the
//! next launch restored it onto the same display — or, when that display had
//! gone, onto whichever one remained, clamped so that no edge of the window was
//! out of reach. Without the clamp a window last seen on an unplugged monitor
//! opens off screen, and the only way back is the Window menu.
//!
//! The decision is pure and lives here; the window plumbing is in `lib.rs`.

use fx_store::PersistedWindowFrame;
use gpui::{Bounds, Pixels, Size, point, px, size};
use uuid::Uuid;

/// Where a window opens with nothing saved: the frame the app always used.
pub const DEFAULT_FRAME: PersistedWindowFrame = PersistedWindowFrame {
    display_id: None,
    x: 80.0,
    y: 80.0,
    width: 1280.0,
    height: 900.0,
};

/// The smallest the window may be made.
pub const MINIMUM_SIZE: (f32, f32) = (1000.0, 640.0);

/// A connected display, as much of it as the placement needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    pub id: Option<Uuid>,
    /// The display's own area. A window's frame is measured from its origin.
    pub bounds: Bounds<Pixels>,
}

/// Where to open the window: which of `displays`, and the frame on it.
///
/// `primary` is the display to fall back to, as an index into `displays`. The
/// saved display wins when it is still connected; otherwise the frame keeps its
/// size and offset but lands on the primary display. Either way the result is
/// clamped to the display, shrinking the window no further than the minimum
/// size unless the display itself is smaller. `None` only when nothing is
/// connected, which leaves the choice to the system.
pub fn placement(
    saved: Option<&PersistedWindowFrame>,
    displays: &[Display],
    primary: usize,
) -> Option<(usize, Bounds<Pixels>)> {
    let frame = saved
        .filter(|frame| frame.is_usable())
        .unwrap_or(&DEFAULT_FRAME);
    let index = frame
        .display_id
        .and_then(|id| displays.iter().position(|display| display.id == Some(id)))
        .or_else(|| (primary < displays.len()).then_some(primary))
        .or_else(|| (!displays.is_empty()).then_some(0))?;
    let area = displays[index].bounds;
    let (area_width, area_height) = (f32::from(area.size.width), f32::from(area.size.height));
    let width = clamp(frame.width, MINIMUM_SIZE.0.min(area_width), area_width);
    let height = clamp(frame.height, MINIMUM_SIZE.1.min(area_height), area_height);
    let x = clamp(frame.x, 0.0, area_width - width);
    let y = clamp(frame.y, 0.0, area_height - height);
    Some((
        index,
        Bounds::new(
            point(area.origin.x + px(x), area.origin.y + px(y)),
            size(px(width), px(height)),
        ),
    ))
}

/// The frame to record for a window at `bounds` on `display`.
///
/// The inverse of [`placement`]: the window's origin is stored relative to the
/// display it is on.
pub fn record(bounds: Bounds<Pixels>, display: Option<&Display>) -> PersistedWindowFrame {
    let origin = display.map_or_else(|| point(px(0.0), px(0.0)), |display| display.bounds.origin);
    PersistedWindowFrame {
        display_id: display.and_then(|display| display.id),
        x: f32::from(bounds.origin.x - origin.x),
        y: f32::from(bounds.origin.y - origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    }
}

/// The minimum window size, as GPUI wants it.
pub fn minimum_size() -> Size<Pixels> {
    size(px(MINIMUM_SIZE.0), px(MINIMUM_SIZE.1))
}

/// `value` within `lower..=upper`, or `lower` when the range is empty.
fn clamp(value: f32, lower: f32, upper: f32) -> f32 {
    if upper < lower {
        return lower;
    }
    value.clamp(lower, upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u128, width: f32, height: f32) -> Display {
        Display {
            id: Some(Uuid::from_u128(id)),
            bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(height))),
        }
    }

    fn frame(display: u128, x: f32, y: f32, width: f32, height: f32) -> PersistedWindowFrame {
        PersistedWindowFrame {
            display_id: Some(Uuid::from_u128(display)),
            x,
            y,
            width,
            height,
        }
    }

    fn rect(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
        (
            bounds.origin.x.into(),
            bounds.origin.y.into(),
            bounds.size.width.into(),
            bounds.size.height.into(),
        )
    }

    #[test]
    fn a_saved_frame_on_a_connected_display_is_restored_exactly() {
        let displays = [display(1, 1440.0, 900.0), display(2, 2560.0, 1440.0)];
        let saved = frame(2, 300.0, 120.0, 1600.0, 1000.0);
        let (index, bounds) = placement(Some(&saved), &displays, 0).unwrap();
        assert_eq!(index, 1, "the display it was on, not the primary");
        assert_eq!(rect(bounds), (300.0, 120.0, 1600.0, 1000.0));
        assert_eq!(
            record(bounds, Some(&displays[1])),
            saved,
            "and it records back"
        );
    }

    #[test]
    fn a_frame_from_a_disconnected_display_is_clamped_onto_the_primary() {
        // Last seen on a large external monitor, far to the right.
        let displays = [display(1, 1440.0, 900.0)];
        let saved = frame(9, 2000.0, 500.0, 1800.0, 1200.0);
        let (index, bounds) = placement(Some(&saved), &displays, 0).unwrap();
        assert_eq!(index, 0);
        let (x, y, width, height) = rect(bounds);
        assert_eq!(
            (width, height),
            (1440.0, 900.0),
            "no larger than the display"
        );
        assert_eq!((x, y), (0.0, 0.0), "and wholly on it");
    }

    #[test]
    fn a_window_partly_off_its_display_is_pulled_back_on() {
        let displays = [display(1, 1920.0, 1080.0)];
        let (_, bounds) =
            placement(Some(&frame(1, -300.0, 900.0, 1200.0, 800.0)), &displays, 0).unwrap();
        assert_eq!(rect(bounds), (0.0, 280.0, 1200.0, 800.0));
    }

    #[test]
    fn the_minimum_size_holds_unless_the_display_is_smaller() {
        let displays = [display(1, 1920.0, 1080.0), display(2, 800.0, 600.0)];
        let (_, bounds) = placement(Some(&frame(1, 0.0, 0.0, 300.0, 200.0)), &displays, 0).unwrap();
        assert_eq!(rect(bounds).2, MINIMUM_SIZE.0);
        assert_eq!(rect(bounds).3, MINIMUM_SIZE.1);
        let (_, bounds) = placement(Some(&frame(2, 0.0, 0.0, 300.0, 200.0)), &displays, 0).unwrap();
        assert_eq!((rect(bounds).2, rect(bounds).3), (800.0, 600.0));
    }

    #[test]
    fn nothing_saved_or_a_broken_frame_opens_at_the_default() {
        let displays = [display(1, 1920.0, 1080.0)];
        let (_, bounds) = placement(None, &displays, 0).unwrap();
        assert_eq!(rect(bounds), (80.0, 80.0, 1280.0, 900.0));
        let broken = frame(1, f32::NAN, 0.0, 1200.0, 800.0);
        let (_, bounds) = placement(Some(&broken), &displays, 0).unwrap();
        assert_eq!(rect(bounds), (80.0, 80.0, 1280.0, 900.0));
        // A small display still gets a window that fits.
        let (_, bounds) = placement(None, &[display(1, 1024.0, 700.0)], 0).unwrap();
        assert_eq!(rect(bounds), (0.0, 0.0, 1024.0, 700.0));
        assert!(placement(None, &[], 0).is_none());
    }

    #[test]
    fn a_display_with_its_own_origin_measures_the_frame_from_it() {
        let offset = Display {
            id: Some(Uuid::from_u128(3)),
            bounds: Bounds::new(point(px(1440.0), px(-200.0)), size(px(1920.0), px(1080.0))),
        };
        let saved = frame(3, 100.0, 50.0, 1200.0, 800.0);
        let (_, bounds) = placement(Some(&saved), &[offset], 0).unwrap();
        assert_eq!(rect(bounds), (1540.0, -150.0, 1200.0, 800.0));
        assert_eq!(record(bounds, Some(&offset)), saved);
    }
}

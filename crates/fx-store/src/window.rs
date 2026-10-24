//! Where the window was, so the next launch can put it back.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The main window's frame, in points, relative to the display it was on.
///
/// Relative rather than global because a display's place in the arrangement
/// can change between launches while the window's place *on* that display is
/// what a person remembers. The display is named by its stable id, not by the
/// index the system happens to give it this session.
///
/// Only a windowed frame is recorded. A full-screen window keeps the frame it
/// had before, which is also what it returns to on leaving full screen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedWindowFrame {
    /// The display the window was on. `None` when the system would not say,
    /// which the restore treats like a display that is no longer connected.
    #[serde(default)]
    pub display_id: Option<Uuid>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PersistedWindowFrame {
    /// Whether the numbers describe a window at all. A frame that does not is
    /// ignored rather than clamped into something nobody chose.
    pub fn is_usable(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite())
            && self.width > 0.0
            && self.height > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PersistedApp;

    fn frame() -> PersistedWindowFrame {
        PersistedWindowFrame {
            display_id: Some(Uuid::from_u128(7)),
            x: 40.0,
            y: 60.0,
            width: 1200.0,
            height: 800.0,
        }
    }

    #[test]
    fn a_window_frame_survives_a_round_trip_through_the_snapshot() {
        let app = PersistedApp {
            window_frame: Some(frame()),
            ..Default::default()
        };
        let json = serde_json::to_string(&app).unwrap();
        assert!(json.contains("\"windowFrame\""));
        let back: PersistedApp = serde_json::from_str(&json).unwrap();
        assert_eq!(back.window_frame, Some(frame()));
    }

    #[test]
    fn a_snapshot_without_a_frame_or_with_a_broken_one_still_loads() {
        // Written before the frame existed.
        let back: PersistedApp = serde_json::from_str("{}").unwrap();
        assert_eq!(back.window_frame, None);
        // A malformed frame loses the frame, not the projects beside it.
        let back: PersistedApp =
            serde_json::from_str(r#"{"windowFrame":"elsewhere","projects":[]}"#).unwrap();
        assert_eq!(back.window_frame, None);
    }

    #[test]
    fn a_frame_without_area_or_with_a_non_finite_edge_is_not_usable() {
        assert!(frame().is_usable());
        for broken in [
            PersistedWindowFrame {
                width: 0.0,
                ..frame()
            },
            PersistedWindowFrame {
                height: -1.0,
                ..frame()
            },
            PersistedWindowFrame {
                x: f32::NAN,
                ..frame()
            },
            PersistedWindowFrame {
                y: f32::INFINITY,
                ..frame()
            },
        ] {
            assert!(!broken.is_usable(), "{broken:?}");
        }
    }
}

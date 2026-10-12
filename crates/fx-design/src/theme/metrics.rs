//! The 8px baseline grid, plus the derived spacing constants that are really
//! just aliases for it.
//!
//! Every gap, pad, and inset in the app is one of these. Add a step here rather
//! than writing a raw number in a view.

/// Spacing scale, from hairline to roomy.
pub struct Spacing;

impl Spacing {
    /// 2px. Between a dot and its label.
    pub const XXXS: f32 = 2.0;
    /// 4px. Tight inline gaps.
    pub const XXS: f32 = 4.0;
    /// 6px. Inside compact controls.
    pub const XS: f32 = 6.0;
    /// 8px. The baseline unit: control padding, list gaps.
    pub const SM: f32 = 8.0;
    /// 12px. Default padding between grouped elements.
    pub const MD: f32 = 12.0;
    /// 16px. Section padding.
    pub const LG: f32 = 16.0;
    /// 20px. Card padding.
    pub const XL: f32 = 20.0;
    /// 24px. Outer panel padding.
    pub const XXL: f32 = 24.0;
    /// 32px. Between major regions.
    pub const XXXL: f32 = 32.0;
    /// 48px. Empty-state breathing room.
    pub const HUGE: f32 = 48.0;

    /// The whole scale, smallest first. Used by tests and by any code that
    /// iterates the scale.
    pub const ALL: [f32; 10] = [
        Self::XXXS,
        Self::XXS,
        Self::XS,
        Self::SM,
        Self::MD,
        Self::LG,
        Self::XL,
        Self::XXL,
        Self::XXXL,
        Self::HUGE,
    ];
}

/// Corner radii. Smaller is tighter.
pub struct Radii;

impl Radii {
    /// 4px. Tags and the tightest chips.
    pub const XS: f32 = 4.0;
    /// 6px. Buttons and inputs.
    pub const SM: f32 = 6.0;
    /// 8px. Cards and the segmented control track.
    pub const MD: f32 = 8.0;
    /// 10px. Larger cards.
    pub const LG: f32 = 10.0;
    /// 12px. Dropdown panels.
    pub const XL: f32 = 12.0;
    /// 16px. Dialogs and the command palette.
    pub const XXL: f32 = 16.0;

    pub const ALL: [f32; 6] = [Self::XS, Self::SM, Self::MD, Self::LG, Self::XL, Self::XXL];
}

/// Stroke widths for borders around cards, fields, menus, and controls.
pub struct BorderWidth;

impl BorderWidth {
    /// The default FlowX edge: a half-point line that reads as a crisp single
    /// pixel on Retina displays.
    pub const HAIRLINE: f32 = 0.5;
    /// Focused or selected fields.
    pub const REGULAR: f32 = 1.0;
    /// Active drop targets and other states that must stand out.
    pub const STRONG: f32 = 1.5;

    pub const ALL: [f32; 3] = [Self::HAIRLINE, Self::REGULAR, Self::STRONG];
}

/// Elevation recipes for surfaces that float above the shell.
///
/// Colors derive from `Colors::overlay`, so depth adapts to light and dark
/// automatically. Apply with the [`crate::fx_shadow`] helper rather than
/// writing a raw `shadow(...)` call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowStyle {
    /// Base color; the caller may scale its alpha further.
    pub color: gpui::Rgba,
    pub blur: f32,
    pub x: f32,
    pub y: f32,
}

impl ShadowStyle {
    pub const fn new(color: gpui::Rgba, blur: f32, x: f32, y: f32) -> Self {
        Self { color, blur, x, y }
    }

    /// The same recipe at a different alpha.
    pub const fn faded(self, factor: f32) -> Self {
        Self {
            color: gpui::Rgba {
                a: self.color.a * factor,
                ..self.color
            },
            ..self
        }
    }
}

pub struct Shadow;

impl Shadow {
    /// Dropdowns and context menus.
    pub fn popover(overlay: gpui::Rgba) -> ShadowStyle {
        ShadowStyle::new(overlay, 18.0, 0.0, 10.0).faded(0.35)
    }

    /// Centered confirmation and rename dialogs.
    pub fn dialog(overlay: gpui::Rgba) -> ShadowStyle {
        ShadowStyle::new(overlay, 24.0, 0.0, 14.0)
    }

    /// The command palette, the highest floating surface.
    pub fn palette(overlay: gpui::Rgba) -> ShadowStyle {
        ShadowStyle::new(overlay, 28.0, 0.0, 16.0)
    }

    /// Panels that slide over content from the trailing edge.
    pub fn trailing_panel(overlay: gpui::Rgba) -> ShadowStyle {
        ShadowStyle::new(overlay, 18.0, -4.0, 0.0).faded(0.24)
    }
}

/// Shell dimensions. Anything that fixes the size of a region belongs here so
/// the shell, the panels, and the overlays cannot drift apart.
pub struct Layout;

impl Layout {
    // Shell
    pub const TITLE_BAR_HEIGHT: f32 = 44.0;
    pub const SIDEBAR_WIDTH: f32 = 260.0;
    pub const SETTINGS_PANEL_WIDTH: f32 = 420.0;
    pub const COMMAND_PALETTE_WIDTH: f32 = 560.0;
    pub const DIALOG_WIDTH: f32 = 440.0;
    /// Row action menus: context menus and the "…" and "+" dropdowns beside them.
    pub const MENU_WIDTH: f32 = 220.0;

    // Right inspector
    pub const RIGHT_PANEL_DEFAULT_WIDTH: f32 = 760.0;
    pub const RIGHT_PANEL_MIN_WIDTH: f32 = 560.0;
    pub const RIGHT_PANEL_MAX_WIDTH: f32 = 1100.0;

    // Content and panels
    /// The comfortable measure for a transcript column.
    pub const READABLE_CONTENT_WIDTH: f32 = 760.0;
    pub const USER_MESSAGE_MAX_WIDTH: f32 = 600.0;
    pub const USER_ATTACHMENT_THUMBNAIL_WIDTH: f32 = 176.0;
    pub const USER_ATTACHMENT_ASPECT_RATIO: f32 = 1.6;
    pub const COLLAPSED_USER_MESSAGE_HEIGHT: f32 = 176.0;
    pub const IMAGE_PREVIEW_MINIMUM_WIDTH: f32 = 720.0;
    pub const IMAGE_PREVIEW_MINIMUM_HEIGHT: f32 = 520.0;
    pub const MINIMUM_CONVERSATION_WIDTH: f32 = 460.0;
    pub const MINIMUM_BROWSER_PREVIEW_WIDTH: f32 = 320.0;
    pub const SPLIT_PANEL_RESIZE_HANDLE_WIDTH: f32 = 12.0;
    pub const MINIMUM_TERMINAL_HEIGHT: f32 = 120.0;
    pub const MAXIMUM_TERMINAL_HEIGHT: f32 = 500.0;
    // Menus and popovers
    /// Height of one menu row. 28pt is the smallest edge that is comfortable
    /// to hit with a pointer.
    pub const MENU_ROW_HEIGHT: f32 = 28.0;
    /// Tallest menu panel before it starts scrolling.
    pub const MENU_MAX_PANEL_HEIGHT: f32 = 320.0;
    /// Width of the checkmark gutter reserved on every menu row, so titles line
    /// up whether or not a section has a selected item.
    pub const MENU_CHECK_GUTTER: f32 = 16.0;
    /// Gap between a menu's anchor and its panel.
    pub const MENU_ANCHOR_GAP: f32 = Spacing::SM;
    /// Margin kept between a panel and the window edge it is clamped to.
    pub const MENU_EDGE_MARGIN: f32 = 8.0;

    pub const DIFF_NAVIGATOR_WIDTH: f32 = 248.0;
    /// Below this the diff navigator stacks above the diff instead of sitting
    /// beside it.
    pub const DIFF_NAVIGATOR_SIDE_BY_SIDE_WIDTH: f32 = 720.0;
    pub const DIFF_NAVIGATOR_COMPACT_HEIGHT: f32 = 200.0;

    /// Composer auto-grow bounds.
    pub const COMPOSER_MIN_HEIGHT: f32 = 28.0;
    pub const COMPOSER_MAX_HEIGHT: f32 = 120.0;

    // Diff rows
    /// Height of a file header row in the diff list.
    pub const DIFF_HEADER_ROW_HEIGHT: f32 = 62.0;

    /// Height of one terminal row.
    pub const TERMINAL_ROW_HEIGHT: f32 = 18.0;
}

impl Layout {
    /// Derived thumbnail height, kept next to its inputs.
    pub const fn user_attachment_thumbnail_height() -> f32 {
        Self::USER_ATTACHMENT_THUMBNAIL_WIDTH / Self::USER_ATTACHMENT_ASPECT_RATIO
    }

    /// Line height for a diff code row: one mono line plus a little leading.
    pub fn diff_line_height(terminal_point_size: f32) -> f32 {
        (terminal_point_size * 1.4).ceil() + Spacing::XXXS
    }

    /// Clamp a persisted or user-dragged panel width into the allowed range.
    pub fn clamp_right_panel_width(width: f32) -> f32 {
        width.clamp(Self::RIGHT_PANEL_MIN_WIDTH, Self::RIGHT_PANEL_MAX_WIDTH)
    }

    /// Clamp a persisted terminal height.
    pub fn clamp_terminal_height(height: f32) -> f32 {
        height.clamp(Self::MINIMUM_TERMINAL_HEIGHT, Self::MAXIMUM_TERMINAL_HEIGHT)
    }

    /// Clamp a persisted split ratio.
    pub fn clamp_split_ratio(ratio: f32) -> f32 {
        ratio.clamp(0.2, 0.8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_is_strictly_increasing() {
        assert!(
            Spacing::ALL.windows(2).all(|w| w[0] < w[1]),
            "{:?}",
            Spacing::ALL
        );
    }

    #[test]
    fn radii_are_strictly_increasing() {
        assert!(
            Radii::ALL.windows(2).all(|w| w[0] < w[1]),
            "{:?}",
            Radii::ALL
        );
    }

    #[test]
    fn border_widths_are_strictly_increasing() {
        assert!(
            BorderWidth::ALL.windows(2).all(|w| w[0] < w[1]),
            "{:?}",
            BorderWidth::ALL
        );
    }

    #[test]
    fn shadow_fading_only_reduces_alpha() {
        let base = gpui::Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.5,
        };
        let faded = ShadowStyle::new(base, 18.0, 0.0, 0.0).faded(0.35);
        assert!((faded.color.a - 0.175).abs() < 1e-6);
        assert_eq!(faded.blur, 18.0);
    }

    #[test]
    fn panel_width_clamps_to_the_documented_range() {
        assert_eq!(Layout::clamp_right_panel_width(10.0), 560.0);
        assert_eq!(Layout::clamp_right_panel_width(9999.0), 1100.0);
        assert_eq!(Layout::clamp_right_panel_width(760.0), 760.0);
    }

    #[test]
    fn terminal_height_and_split_ratio_clamp() {
        assert_eq!(Layout::clamp_terminal_height(10.0), 120.0);
        assert_eq!(Layout::clamp_terminal_height(9000.0), 500.0);
        assert_eq!(Layout::clamp_split_ratio(0.0), 0.2);
        assert_eq!(Layout::clamp_split_ratio(1.0), 0.8);
    }

    #[test]
    fn thumbnail_height_is_derived_from_width_and_ratio() {
        assert!((Layout::user_attachment_thumbnail_height() - 110.0).abs() < 1e-6);
    }
}

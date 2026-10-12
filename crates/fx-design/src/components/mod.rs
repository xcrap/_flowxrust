//! Shared controls.
//!
//! Implement UI by extending this module, not by styling each screen ad hoc.
//! Every component resolves its colors, sizes, and radii from the token layer,
//! so a theme change reaches all of them at once.

pub mod button;
pub mod context_menu;
pub mod menu;
pub mod menu_model;
pub mod segmented;
pub mod status;
pub mod surface;
pub mod tooltip;

pub use context_menu::{ContextRegion, ContextRegistry, handle_root_right_click, open_at};
pub use menu::{
    MenuLayer, MenuTrigger, RowPalette, menu_layer_overlay, menu_overlay, menu_surface,
    menu_trigger, row_palette,
};
pub use menu_model::{MenuItem, MenuItemTone, MenuModel, MenuSection};

pub use button::{
    ButtonStyle, ICON_BUTTON_SIZE, IconButtonProps, IconButtonVariant, button, button_palette,
    button_with_icon, compact_action_button, icon_button, icon_button_palette,
};
pub use segmented::{SegmentedOption, segmented_control};
pub use status::{
    ActivityState, BadgeTone, activity_dot, badge, context_ring_color, context_ring_icon,
    pulsing_dot, spinner_arc, static_ring, typing_indicator,
};
pub use surface::{
    DividerAxis, card, card_default, divider, divider_with_axis, empty_state, inline_notice, panel,
    section_divider, section_header,
};
pub use tooltip::tooltip;

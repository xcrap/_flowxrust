//! Shared controls.
//!
//! Implement UI by extending this module, not by styling each screen ad hoc.
//! Every component resolves its colors, sizes, and radii from the token layer,
//! so a theme change reaches all of them at once.

pub mod attachment;
pub mod button;
pub mod change_indicator;
pub mod checkbox;
pub mod checklist;
pub mod collapsible;
pub mod context_menu;
pub mod context_meter;
pub mod menu;
pub mod menu_model;
pub mod palette;
pub mod provider_status;
pub mod reference;
pub mod rich_text;
pub mod segmented;
pub mod settings;
pub mod status;
pub mod surface;
pub mod tooltip;
pub mod turn_action;

pub use change_indicator::{
    ChangeIndicatorPalette, change_indicator, change_indicator_palette, change_indicator_surface,
};
pub use context_meter::{ContextUsage, context_meter};
pub use provider_status::{ProviderIdentity, provider_status};
pub use turn_action::{TurnAction, turn_action_button};

pub use collapsible::collapsible;
pub use context_menu::{
    ContextRegion, ContextRegistry, context_frame, context_region, handle_root_right_click, open_at,
};
pub use menu::{
    MenuLayer, MenuTrigger, RowPalette, menu_layer_overlay, menu_overlay, menu_surface,
    menu_trigger, row_palette,
};
pub use menu_model::{MenuItem, MenuItemTone, MenuModel, MenuSection};
pub use palette::{
    PaletteRow, PaletteRowPalette, palette_panel, palette_row, palette_row_palette,
    palette_surface, shortcut_chip,
};

pub use button::{
    ButtonStyle, ICON_BUTTON_SIZE, IconButtonProps, IconButtonVariant, button, button_palette,
    button_with_icon, choice_row, compact_action_button, compact_action_palette,
    compact_action_toggle, glyph_button, icon_button, icon_button_palette, navigation_button,
    navigation_palette,
};
pub use checkbox::{CheckboxPalette, checkbox, checkbox_palette};
pub use checklist::{
    ChecklistInk, ChecklistMark, checklist, checklist_ink, checklist_row, checklist_surface,
};
pub use segmented::{SegmentedOption, segmented_control};
pub use settings::{
    SettingsGroupPalette, SwitchPalette, breadcrumb, settings_group, settings_group_palette,
    settings_nav_label, settings_row, settings_row_custom, settings_row_dense, settings_row_label,
    settings_search, settings_section, settings_section_with_action, switch, switch_palette,
};
pub use status::{
    ActivityState, BadgeTone, activity_dot, badge, context_ring_color, context_ring_icon,
    pulsing_dot, spinner_arc, static_ring, typing_indicator,
};
pub use surface::{
    DividerAxis, card, card_default, divider, divider_with_axis, empty_state, inline_notice, panel,
    section_divider, section_header, titlebar_control, window_title,
};
pub use tooltip::tooltip;

//! The FlowX token layer.
//!
//! Every visual constant in the app resolves from a [`Theme`]. Views read
//! tokens; they never construct colors, sizes, or durations themselves.

pub mod animation;
pub mod color;
pub mod global;
pub mod metrics;
pub mod typography;

pub use animation::FxAnimation;
pub use color::{
    AccentColor, AppearanceMode, BaseTone, Colors, Palette, Rgba, TextSizePreset, Theme,
    ThemeSignature, ToneScale, WithAlpha, composite, contrast, contrast_ratio, h, ha,
    relative_luminance,
};
pub use global::{ThemeGuard, current, set, set_system_appearance, update};
pub use metrics::{BorderWidth, Layout, Radii, Shadow, ShadowStyle, Spacing};
pub use typography::{FAMILY_MONO, FAMILY_ROUNDED, IconSize, TextRole, TextStyle, Typography};

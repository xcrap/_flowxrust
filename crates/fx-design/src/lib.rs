//! `fx-design` — the FlowX design system.
//!
//! This crate is the single source of truth for how FlowX looks. Nothing else
//! in the workspace should construct a color, pick a font size, choose a radius,
//! or write an animation duration. If a component is missing, add it here first
//! and consume it from there.
//!
//! # Layout
//!
//! - [`theme`] — every token: colors, type, spacing, radii, borders, shadows,
//!   layout metrics, and animation presets.
//! - [`styled`] — chained token accessors for GPUI elements, plus the helpers
//!   that hide GPUI 0.2.2's style-refinement shape.
//! - [`components`] — the shared control vocabulary.
//! - [`icon`] — the icon set.
//!
//! # Rules
//!
//! - Views read semantic tokens (`Colors::fg`, `Colors::diff_added_bg`), never
//!   literals. Add a token if one is missing.
//! - No native menus, no vibrancy, no translucent window chrome. FlowX uses
//!   opaque custom backgrounds and its own flat menus.
//! - Diffs use `diff_added_bg` / `diff_removed_bg`, never a semantic color at
//!   low opacity.
//! - `fg_quaternary` is for disabled states and decorative glyphs only.

pub mod components;
pub mod icon;
pub mod styled;
pub mod theme;

pub use styled::{Alignment, FxRefinement, FxStyled, Placement};
pub use theme::{
    AccentColor, AppearanceMode, BaseTone, BorderWidth, Colors, FxAnimation, IconSize, Layout,
    Palette, Radii, Shadow, ShadowStyle, Spacing, TextRole, TextSizePreset, TextStyle, Theme,
    ThemeSignature, Typography, WithAlpha, composite, contrast, contrast_ratio, relative_luminance,
};

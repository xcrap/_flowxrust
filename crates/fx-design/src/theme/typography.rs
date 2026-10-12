//! Type scale, font families, and icon sizing.
//!
//! Every size is multiplied by `Theme::text_scale`, so a single
//! `TextSizePreset` change rescales the whole app. Views never write a point
//! size directly — they pick a [`TextRole`].

use gpui::{FontWeight, SharedString};

use super::color::Theme;

/// The rounded system face used by AppKit's `.rounded` design.
///
/// The downloadable "SF Pro Rounded" family is not installed on a stock Mac.
/// CoreText exposes the built-in design under this native family alias, which
/// GPUI's font-kit loader can resolve without falling back to Helvetica.
pub const FAMILY_ROUNDED: &str = ".AppleSystemUIFontRounded";

/// AppKit's monospaced system face for code, terminal output, and diff numbers.
/// Like the rounded design, its built-in family is addressed by the native
/// alias, not the separately installable "SF Mono" family name.
pub const FAMILY_MONO: &str = ".AppleSystemUIFontMonospaced";

/// A resolved text style. Build these with [`Typography::style`] rather than
/// constructing them directly, so the text-size preset stays authoritative.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub family: &'static str,
    /// Already multiplied by the active text scale.
    pub size: f32,
    pub weight: FontWeight,
}

impl TextStyle {
    pub fn family_name(&self) -> SharedString {
        SharedString::from(self.family)
    }

    pub const fn weight(&self) -> FontWeight {
        self.weight
    }
}

/// Named type styles. The Rust counterpart of Swift's `FXTypography.*`
/// statics, minus the `view` modifiers that GPUI expresses as chained calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRole {
    /// Screen-level heading.
    Title1,
    /// Panel and dialog heading.
    Title2,
    /// Section heading inside a panel.
    Title3,
    /// Default reading size.
    Body,
    /// Default size at medium weight, for controls and emphasis.
    BodyMedium,
    /// Secondary supporting text.
    Caption,
    /// Secondary supporting text at medium weight.
    CaptionMedium,
    /// Small uppercase section labels in the sidebar.
    Overline,
    /// Code and inline monospace.
    Mono,
    /// Monospace at caption size, for diff line numbers and counts.
    MonoSmall,
}

impl TextRole {
    const fn spec(self) -> (&'static str, f32, f32) {
        match self {
            TextRole::Title1 => (FAMILY_ROUNDED, 26.0, 600.0),
            TextRole::Title2 => (FAMILY_ROUNDED, 20.0, 600.0),
            TextRole::Title3 => (FAMILY_ROUNDED, 16.0, 500.0),
            TextRole::Body => (FAMILY_ROUNDED, 14.0, 400.0),
            TextRole::BodyMedium => (FAMILY_ROUNDED, 14.0, 500.0),
            TextRole::Caption => (FAMILY_ROUNDED, 12.0, 400.0),
            TextRole::CaptionMedium => (FAMILY_ROUNDED, 12.0, 500.0),
            TextRole::Overline => (FAMILY_ROUNDED, 11.0, 600.0),
            TextRole::Mono => (FAMILY_MONO, 13.0, 400.0),
            TextRole::MonoSmall => (FAMILY_MONO, 12.0, 400.0),
        }
    }

    /// Line height as a multiple of the font size.
    ///
    /// GPUI has no automatic line height, so each role carries one explicitly.
    /// Tighter for headings, looser for body copy.
    pub const fn line_height_multiplier(self) -> f32 {
        match self {
            TextRole::Title1 => 1.20,
            TextRole::Title2 => 1.22,
            TextRole::Title3 => 1.28,
            TextRole::Body => 1.45,
            TextRole::BodyMedium => 1.45,
            TextRole::Caption => 1.40,
            TextRole::CaptionMedium => 1.40,
            TextRole::Overline => 1.30,
            // Monospace sits on an exact grid so diff rows and terminal cells
            // stay aligned vertically.
            TextRole::Mono => 1.40,
            TextRole::MonoSmall => 1.35,
        }
    }

    /// Whether text in this role should use tight tracking. Overline labels
    /// need positive tracking; long-form body copy needs none.
    pub const fn letter_spacing(self) -> f32 {
        match self {
            TextRole::Overline => 0.35,
            TextRole::Title1 => -0.2,
            TextRole::Title2 => -0.1,
            _ => 0.0,
        }
    }
}

/// Icon glyph sizes. The Rust counterpart of `FXIconSize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconSize {
    /// Inside a badge or a dense list row.
    Micro,
    /// Default size for a control's icon.
    Small,
    /// Default size for a standalone icon.
    Regular,
    /// Icons inside standard-height controls.
    Control,
    Medium,
    Large,
    /// Empty-state illustrations.
    Illustration,
    /// Hero empty-state illustrations.
    Action,
    /// Oversized decorative icons.
    Hero,
}

impl IconSize {
    const fn spec(self) -> (f32, f32) {
        match self {
            IconSize::Micro => (9.0, 600.0),
            IconSize::Small => (11.0, 500.0),
            IconSize::Regular => (12.0, 500.0),
            IconSize::Control => (13.0, 500.0),
            IconSize::Medium => (14.0, 500.0),
            IconSize::Large => (18.0, 500.0),
            IconSize::Illustration => (22.0, 400.0),
            IconSize::Action => (26.0, 400.0),
            IconSize::Hero => (44.0, 100.0),
        }
    }
}

pub struct Typography;

impl Typography {
    /// Resolve a type role against the active theme.
    pub fn style(role: TextRole, theme: &Theme) -> TextStyle {
        let (family, base, weight) = role.spec();
        TextStyle {
            family,
            size: base * theme.text_scale(),
            weight: FontWeight(weight),
        }
    }

    /// Resolve an icon size against the active theme.
    pub fn icon(size: IconSize, theme: &Theme) -> TextStyle {
        let (base, weight) = size.spec();
        TextStyle {
            family: FAMILY_ROUNDED,
            size: base * theme.text_scale(),
            weight: FontWeight(weight),
        }
    }

    /// Point size for the body role. Callers that lay out rows of a fixed
    /// height (the diff list, terminal cells) derive that height from this
    /// rather than hardcoding a number.
    pub fn body_point_size(theme: &Theme) -> f32 {
        14.0 * theme.text_scale()
    }

    /// Point size for the mono role, matching `TextRole::Mono`.
    pub fn terminal_point_size(theme: &Theme) -> f32 {
        13.0 * theme.text_scale()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::color::{TextSizePreset, Theme};

    #[test]
    fn text_scale_multiplies_every_role() {
        let standard = Theme::default();
        let large = Theme {
            text_size: TextSizePreset::Large,
            ..standard
        };
        for role in [
            TextRole::Title1,
            TextRole::Body,
            TextRole::Caption,
            TextRole::Mono,
            TextRole::MonoSmall,
        ] {
            let a = Typography::style(role, &standard);
            let b = Typography::style(role, &large);
            assert!(
                b.size > a.size,
                "{role:?} did not grow: {} -> {}",
                a.size,
                b.size
            );
            assert_eq!(a.family, b.family, "{role:?} changed family with text size");
        }
    }

    #[test]
    fn roles_descend_from_title_to_caption() {
        let t = Theme::default();
        let size = |r| Typography::style(r, &t).size;
        assert!(size(TextRole::Title1) > size(TextRole::Title2));
        assert!(size(TextRole::Title2) > size(TextRole::Title3));
        assert!(size(TextRole::Title3) > size(TextRole::Body));
        assert!(size(TextRole::Body) > size(TextRole::Caption));
    }

    #[test]
    fn mono_roles_use_the_monospaced_family() {
        let t = Theme::default();
        for role in [TextRole::Mono, TextRole::MonoSmall] {
            assert_eq!(Typography::style(role, &t).family, FAMILY_MONO);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_families_resolve_without_gpui_falling_back_to_a_different_face() {
        // Use the same native loader as GPUI. TestAppContext's fake text system
        // accepts missing families, hiding the fallback to proportional text.
        let source = font_kit::source::SystemSource::new();
        for family in [FAMILY_ROUNDED, FAMILY_MONO] {
            let handles = source.select_family_by_name(family).unwrap();
            let faces: Vec<_> = handles
                .fonts()
                .iter()
                .map(|handle| handle.load().unwrap())
                .collect();
            let properties: Vec<_> = faces.iter().map(|face| face.properties()).collect();
            for weight in [400.0, 500.0, 600.0] {
                let index = font_kit::matching::find_best_match(
                    &properties,
                    &font_kit::properties::Properties {
                        weight: font_kit::properties::Weight(weight),
                        ..Default::default()
                    },
                )
                .unwrap();
                let face = &faces[index];
                assert_eq!(
                    face.family_name(),
                    family,
                    "{family} at weight {weight} silently fell back"
                );
                if family == FAMILY_MONO {
                    assert!(face.is_monospace());
                    let narrow = face.advance(face.glyph_for_char('i').unwrap()).unwrap();
                    let wide = face.advance(face.glyph_for_char('W').unwrap()).unwrap();
                    assert_eq!(narrow.x(), wide.x(), "code must use equal-width glyphs");
                }
            }
        }
    }

    #[test]
    fn icon_sizes_are_strictly_ordered() {
        let t = Theme::default();
        let sizes = [
            IconSize::Micro,
            IconSize::Small,
            IconSize::Regular,
            IconSize::Control,
            IconSize::Medium,
            IconSize::Large,
            IconSize::Illustration,
            IconSize::Action,
            IconSize::Hero,
        ]
        .map(|s| Typography::icon(s, &t).size);
        assert!(sizes.windows(2).all(|w| w[0] < w[1]), "{sizes:?}");
    }

    #[test]
    fn point_size_helpers_agree_with_their_roles() {
        let t = Theme::default();
        assert_eq!(
            Typography::body_point_size(&t),
            Typography::style(TextRole::Body, &t).size
        );
        assert_eq!(
            Typography::terminal_point_size(&t),
            Typography::style(TextRole::Mono, &t).size
        );
    }
}

//! The FlowX icon set.
//!
//! Icons use the same names as the SF Symbols the Swift app used, so call sites
//! port without renaming. Glyphs are Lucide (ISC), embedded as complete SVG
//! documents by `build.rs` and supplied to GPUI through [`IconAssets`].
//!
//! One deliberate difference from SF Symbols: several Swift call sites used
//! `.fill` variants (`stop.circle.fill`, `lock.shield.fill`). Lucide is
//! stroke-only, and FlowX's design system is built on flat strokes, so those
//! names render as outlines. That reads as more consistent inside this UI
//! rather than less.

use std::borrow::Cow;

use gpui::{AssetSource, IntoElement, Rgba, SharedString, Styled, div, svg};

use crate::theme::{IconSize, TextRole, Typography, color::Theme, global};

include!(concat!(env!("OUT_DIR"), "/icon_paths.rs"));

/// Embedded assets for the FlowX design system.
///
/// Install with `Application::new().with_assets(IconAssets)` before opening a
/// window. No filesystem paths or resources next to the executable are needed.
#[derive(Clone, Copy, Debug, Default)]
pub struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let Some(name) = path
            .strip_prefix("icons/")
            .and_then(|name| name.strip_suffix(".svg"))
        else {
            return Ok(None);
        };
        // The tables are sorted by symbol name. Appending `.svg` changes the
        // order of prefix names such as arrow.clockwise / arrow.clockwise.circle.
        Ok(ICON_PATHS
            .binary_search_by_key(&name, |(key, _)| key)
            .ok()
            .map(|index| Cow::Borrowed(ICON_ASSETS[index].1)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let prefix = path.trim_end_matches('/');
        Ok(ICON_ASSETS
            .iter()
            .filter(|(key, _)| {
                prefix.is_empty()
                    || key
                        .strip_prefix(prefix)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
            .map(|(key, _)| SharedString::from(*key))
            .collect())
    }
}

/// A named glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Icon {
    /// The SF Symbols spelling, e.g. `chevron.down`.
    name: &'static str,
}

impl Icon {
    /// Look up a glyph by its SF Symbols name.
    ///
    /// Unknown names resolve to nothing at draw time rather than panicking, so
    /// a typo in a new view degrades to an invisible icon instead of a crash.
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Path data for this glyph, if the name is in the set.
    pub fn path(&self) -> Option<&'static str> {
        ICON_PATHS
            .binary_search_by_key(&self.name, |(k, _)| k)
            .ok()
            .map(|i| ICON_PATHS[i].1)
    }

    pub fn exists(&self) -> bool {
        self.path().is_some()
    }

    /// The key GPUI resolves through [`IconAssets`]. This is distinct from the
    /// SVG drawing instructions returned by [`Self::path`].
    pub fn asset_path(&self) -> Option<&'static str> {
        ICON_PATHS
            .binary_search_by_key(&self.name, |(key, _)| key)
            .ok()
            .map(|index| ICON_ASSETS[index].0)
    }

    /// The icon as a GPUI element at the given size, painted in `color`.
    ///
    /// `Svg` takes its paint from the element's text color, so this sets
    /// `text_color` rather than a fill. An unknown name renders an
    /// invisible same-size box, so layout is unaffected and nothing panics.
    pub fn render(&self, size: IconSize, color: Rgba, theme: &Theme) -> gpui::AnyElement {
        let edge = gpui::px(Typography::icon(size, theme).size);
        match self.asset_path() {
            Some(path) => svg()
                .path(path)
                .size(edge)
                .text_color(color)
                .into_any_element(),
            None => div().size(edge).into_any_element(),
        }
    }

    /// Every glyph name in the set. Used by tests and by a future icon
    /// browser.
    pub fn all_names() -> impl Iterator<Item = &'static str> {
        ICON_PATHS.iter().map(|(k, _)| *k)
    }
}

/// Generate the `Icon` constants that the app refers to by name.
///
/// Each becomes `Icon::CHEVRON_DOWN`, matching the SF Symbol it replaces.
macro_rules! icons {
    ($($konst:ident => $name:literal),* $(,)?) => {
        impl Icon {
            $(#[doc = concat!("`", $name, "`")] pub const $konst: Icon = Icon { name: $name };)*
        }
    };
}

icons! {
    // Navigation and disclosure
    CHEVRON_LEFT => "chevron.left",
    CHEVRON_RIGHT => "chevron.right",
    CHEVRON_DOWN => "chevron.down",
    ARROW_UP_LEFT_ARROW_DOWN_RIGHT => "arrow.up.left.and.arrow.down.right",
    ARROW_DOWN_RIGHT_ARROW_UP_LEFT => "arrow.down.right.and.arrow.up.left",

    // Actions
    ELLIPSIS => "ellipsis",
    ELLIPSIS_CIRCLE => "ellipsis.circle",
    XMARK => "xmark",
    XMARK_CIRCLE_FILL => "xmark.circle.fill",
    PLUS => "plus",
    PLUS_CIRCLE => "plus.circle",
    CHECKMARK => "checkmark",
    CHECKMARK_CIRCLE_FILL => "checkmark.circle.fill",
    ARROW_UP => "arrow.up",
    ARROW_UP_CIRCLE_FILL => "arrow.up.circle.fill",
    ARROW_UP_RIGHT_SQUARE => "arrow.up.right.square",
    ARROW_CLOCKWISE => "arrow.clockwise",
    ARROW_CLOCKWISE_CIRCLE => "arrow.clockwise.circle",
    ARROW_COUNTERCLOCKWISE => "arrow.counterclockwise",
    ARROW_UTURN_BACKWARD_CIRCLE => "arrow.uturn.backward.circle",
    STOP_FILL => "stop.fill",
    STOP_CIRCLE_FILL => "stop.circle.fill",
    PENCIL => "pencil",
    PENCIL_LINE => "pencil.line",
    TRASH => "trash",
    ARCHIVEBOX => "archivebox",
    TRAY => "tray",
    ERASER => "eraser",
    ERASER_ALT => "eraser",

    // Files, folders, search
    MAGNIFYINGGLASS => "magnifyingglass",
    DOC => "doc",
    DOC_TEXT => "doc.text",
    DOC_ON_DOC => "doc.on.doc",
    DOC_ON_CLIPBOARD => "doc.on.clipboard",
    DOC_TEXT_MAGNIFYINGGLASS => "doc.text.magnifyingglass",
    FOLDER => "folder",
    FOLDER_BADGE_PLUS => "folder.badge.plus",
    PAPERCLIP => "paperclip",
    PHOTO => "photo",
    PHOTO_BADGE_PLUS => "photo.badge.plus",
    PHOTO_BADGE_EXCLAMATIONMARK => "photo.badge.exclamationmark",
    SPARKLE_MAGNIFYINGGLASS => "sparkle.magnifyingglass",

    // Status and state
    EXCLAMATIONMARK_TRIANGLE_FILL => "exclamationmark.triangle.fill",
    EXCLAMATIONMARK_CIRCLE_FILL => "exclamationmark.circle.fill",
    HOURGLASS_BOTTOMHALF_FILLED => "hourglass.bottomhalf.filled",
    LOCK_SHIELD_FILL => "lock.shield.fill",
    HAND_RAISED_FILL => "hand.raised.fill",
    TARGET => "target",

    // Chrome and layout
    GEARSHAPE => "gearshape",
    GLOBE => "globe",
    SIDEBAR_LEFT => "sidebar.left",
    SIDEBAR_RIGHT => "sidebar.right",
    RECTANGLE_SPLIT_3X1 => "rectangle.split.3x1",
    TERMINAL => "terminal",
    WRENCH_AND_SCREWDRIVER => "wrench.and.screwdriver",

    // People and conversation
    QUESTIONMARK_BUBBLE_FILL => "questionmark.bubble.fill",
    PERSON_CROP_CIRCLE => "person.crop.circle",
    BUBBLE_LEFT_AND_BUBBLE_RIGHT => "bubble.left.and.bubble.right",
    ARROW_TRIANGLE_BRANCH => "arrow.triangle.branch",
}

/// An icon painted in `color`, sized from the active theme.
pub fn icon(role: Icon, color: Rgba, size: IconSize) -> gpui::AnyElement {
    role.render(size, color, &global::current())
}

/// An icon sized to sit inline with a text role.
pub fn icon_inline(role: Icon, color: Rgba, text_role: TextRole) -> gpui::AnyElement {
    let size = match text_role {
        TextRole::Overline | TextRole::Caption | TextRole::CaptionMedium => IconSize::Small,
        TextRole::Title1 | TextRole::Title2 => IconSize::Medium,
        _ => IconSize::Regular,
    };
    role.render(size, color, &global::current())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vendored_icon_flattens_to_path_data() {
        let names: Vec<_> = Icon::all_names().collect();
        assert!(names.len() >= 50, "only {} icons vendored", names.len());
        for name in names {
            let icon = Icon::new(name);
            let path = icon.path().unwrap_or_else(|| panic!("{name} has no path"));
            assert!(!path.is_empty(), "{name} produced empty path data");
            // Lucide mixes absolute and relative commands, so either is fine.
            assert!(
                path.starts_with('M') || path.starts_with('m'),
                "{name} does not start with a move"
            );
        }
    }

    #[test]
    fn every_icon_asset_resolves_and_rasterizes_visible_strokes() {
        for name in Icon::all_names() {
            let icon = Icon::new(name);
            let key = icon.asset_path().expect("known icon asset key");
            assert_eq!(key, format!("icons/{name}.svg"));
            let bytes = IconAssets
                .load(key)
                .expect("asset lookup succeeds")
                .unwrap_or_else(|| panic!("{name} has no SVG asset"));
            let tree = resvg::usvg::Tree::from_data(&bytes, &Default::default())
                .unwrap_or_else(|error| panic!("{name} is not a valid SVG: {error}"));
            let mut pixmap = resvg::tiny_skia::Pixmap::new(24, 24).unwrap();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::identity(),
                &mut pixmap.as_mut(),
            );
            assert!(
                pixmap.pixels().iter().any(|pixel| pixel.alpha() > 0),
                "{name} rasterized to an invisible icon"
            );
        }
    }

    #[test]
    fn icon_asset_lookup_is_exact_and_directory_listing_is_scoped() {
        assert!(IconAssets.load("icons/missing.svg").unwrap().is_none());
        assert!(
            IconAssets
                .load(Icon::PLUS.path().unwrap())
                .unwrap()
                .is_none()
        );
        assert!(IconAssets.list("icons-other").unwrap().is_empty());
        let assets = IconAssets.list("icons/").unwrap();
        assert_eq!(assets.len(), Icon::all_names().count());
        assert_eq!(assets, IconAssets.list("icons").unwrap());
        assert_eq!(assets, IconAssets.list("").unwrap());
        assert!(
            assets
                .iter()
                .all(|path| IconAssets.load(path).unwrap().is_some())
        );
    }

    #[test]
    fn the_icon_table_is_sorted_so_lookup_can_binary_search() {
        let sorted = {
            let mut v = ICON_PATHS.to_vec();
            v.sort_by(|a, b| a.0.cmp(b.0));
            v
        };
        assert_eq!(sorted, ICON_PATHS.to_vec(), "icon table is unsorted");
    }

    #[test]
    fn icon_names_are_unique() {
        let mut names: Vec<_> = Icon::all_names().collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate icon names vendored");
    }

    #[test]
    fn constants_all_resolve() {
        let all = [
            Icon::XMARK,
            Icon::CHEVRON_DOWN,
            Icon::FOLDER_BADGE_PLUS,
            Icon::EXCLAMATIONMARK_TRIANGLE_FILL,
            Icon::TERMINAL,
            Icon::GEARSHAPE,
        ];
        for icon in all {
            assert!(
                icon.exists(),
                "{} did not resolve to path data",
                icon.name()
            );
        }
    }

    #[test]
    fn an_unknown_name_is_inert_rather_than_fatal() {
        let icon = Icon::new("this.does.not.exist");
        assert!(!icon.exists());
        assert!(icon.path().is_none());
        assert!(icon.asset_path().is_none());
    }

    /// Scan path data for numbers, honoring a leading sign and a decimal point.
    ///
    /// Splitting on punctuation would tear `9.75` into two tokens, so this
    /// walks the string the way a real path parser would.
    fn path_numbers(d: &str) -> Vec<f64> {
        let bytes: Vec<char> = d.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            // A number may start with a digit, a sign, or a bare decimal point
            // — Lucide writes both `6.828` and `.586`, and `1.414.586` is two
            // numbers, not one.
            let starts = c.is_ascii_digit()
                || ((c == '-' || c == '.') && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit());
            if starts {
                let start = i;
                if bytes[i] == '-' {
                    i += 1;
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == '.' {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if let Ok(v) = bytes[start..i].iter().collect::<String>().parse::<f64>() {
                    out.push(v);
                }
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn the_number_scanner_keeps_decimals_and_signs_intact() {
        let nums = path_numbers("M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1-6.74L3 16");
        assert_eq!(nums.first(), Some(&3.0));
        assert!(nums.contains(&9.75), "9.75 was split: {nums:?}");
        assert!(nums.contains(&-6.74), "-6.74 was split: {nums:?}");
        assert!(nums.iter().all(|v| v.is_finite()));

        // A leading decimal point starts a number, and two adjacent numbers
        // written `.71.71` must not merge into 7171.
        let nums = path_numbers("A.71.71 0 0 1-1.414.586");
        assert!(nums.contains(&0.71), "0.71 was missed: {nums:?}");
        assert!(nums.contains(&-1.414), "-1.414 was missed: {nums:?}");
        assert!(nums.contains(&0.586), "0.586 was missed: {nums:?}");
    }

    #[test]
    fn flattened_paths_stay_inside_the_authored_viewbox() {
        // A coordinate far outside 0..24 means the merge produced garbage.
        // Arcs legitimately extend a little past the viewBox via their control
        // points, so the bound is generous.
        for name in Icon::all_names() {
            let path = Icon::new(name).path().unwrap();
            for v in path_numbers(path) {
                assert!(
                    (-32.0..=48.0).contains(&v),
                    "{name} has an out-of-range coordinate {v} in {path}"
                );
            }
        }
    }
}

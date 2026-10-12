//! Chained token accessors for GPUI elements.
//!
//! These are the Rust counterpart of SwiftUI's `view.fxBody()` family. The goal
//! is that applying the design system reads the same way it did in Swift, while
//! every value still comes from the token layer.
//!
//! ```ignore
//! div().fx_body().text_color(Colors::fg(&theme)).child("Hello")
//! ```

use gpui::{AbsoluteLength, Fill, Hsla, Pixels, Rgba, SharedString, StyleRefinement, Styled};

use crate::theme::{Colors, Layout, Radii, ShadowStyle, TextRole, TextStyle, Typography, global};

/// Shorthand for turning a token value into the length type a style wants.
#[inline]
pub fn px_len(v: f32) -> AbsoluteLength {
    gpui::px(v).into()
}

/// `px` for layout-space measurements.
#[inline]
pub fn p(v: f32) -> Pixels {
    gpui::px(v)
}

/// Token accessors available on every GPUI element.
pub trait FxStyled: Styled + Sized {
    /// Apply a fully resolved text style.
    fn fx_style(self, style: &TextStyle) -> Self {
        self.font_family(SharedString::from(style.family))
            .font_weight(style.weight())
            .text_size(px_len(style.size))
    }

    /// Apply a type role along with its line height.
    fn fx_role(self, role: TextRole) -> Self {
        let theme = global::current();
        let style = Typography::style(role, &theme);
        self.fx_style(&style)
            .line_height(px_len(style.size * role.line_height_multiplier()))
    }

    // ── Common roles, colored for foreground use ──────────────────────────

    /// Screen-level heading, `Colors::fg`.
    fn fx_title1(self) -> Self {
        self.fx_role(TextRole::Title1)
            .text_color(Colors::fg(&global::current()))
    }
    /// Panel and dialog heading.
    fn fx_title2(self) -> Self {
        self.fx_role(TextRole::Title2)
            .text_color(Colors::fg(&global::current()))
    }
    /// Section heading.
    fn fx_title3(self) -> Self {
        self.fx_role(TextRole::Title3)
            .text_color(Colors::fg(&global::current()))
    }
    /// Default reading size.
    fn fx_body(self) -> Self {
        self.fx_role(TextRole::Body)
            .text_color(Colors::fg(&global::current()))
    }
    /// Default size at medium weight.
    fn fx_body_medium(self) -> Self {
        self.fx_role(TextRole::BodyMedium)
            .text_color(Colors::fg(&global::current()))
    }
    /// Secondary supporting text.
    fn fx_caption(self) -> Self {
        self.fx_role(TextRole::Caption)
            .text_color(Colors::fg_secondary(&global::current()))
    }
    /// Small uppercase sidebar section label.
    fn fx_overline(self) -> Self {
        self.fx_role(TextRole::Overline)
            .text_color(Colors::fg_secondary(&global::current()))
    }
    /// Monospace, for code and terminal output.
    fn fx_mono(self) -> Self {
        self.fx_role(TextRole::Mono)
            .text_color(Colors::fg(&global::current()))
    }

    // ── Spacing ────────────────────────────────────────────────────────────

    /// Horizontal padding at a spacing step.
    fn fx_px(self, step: f32) -> Self {
        self.px(gpui::px(step))
    }
    /// Vertical padding at a spacing step.
    fn fx_py(self, step: f32) -> Self {
        self.py(gpui::px(step))
    }
    /// Uniform padding at a spacing step.
    fn fx_pad(self, step: f32) -> Self {
        self.p(gpui::px(step))
    }

    // ── Surfaces ───────────────────────────────────────────────────────────

    /// Card and menu surfaces: `bg_surface` fill, hairline border, `lg` radius.
    fn fx_surface(self) -> Self {
        let t = global::current();
        self.bg(Colors::bg_surface(&t))
            .border_1()
            .border_color(Colors::border(&t))
            .rounded(px_len(Radii::LG))
    }

    /// A floating panel edge: `border_medium` at `xl` radius.
    fn fx_panel(self) -> Self {
        let t = global::current();
        self.bg(Colors::bg_surface(&t))
            .border_1()
            .border_color(Colors::border_medium(&t))
            .rounded(px_len(Radii::XL))
    }

    /// A hairline separator across the current axis.
    fn fx_divider(self) -> Self {
        self.bg(Colors::border(&global::current()))
    }

    // ── Elevation ──────────────────────────────────────────────────────────

    /// Apply an elevation recipe.
    fn fx_shadow(self, style: ShadowStyle) -> Self {
        self.shadow(vec![gpui::BoxShadow {
            color: style.color.into(),
            offset: gpui::point(gpui::px(style.x), gpui::px(style.y)),
            blur_radius: gpui::px(style.blur),
            spread_radius: gpui::px(0.0),
        }])
    }

    // ── Sizing ─────────────────────────────────────────────────────────────

    /// A square control of the given edge length.
    fn fx_square(self, size: f32) -> Self {
        self.size(px_len(size))
    }

    /// The composer, or any other auto-growing text region.
    fn fx_composer_bounds(self) -> Self {
        self.min_h(px_len(Layout::COMPOSER_MIN_HEIGHT))
            .max_h(px_len(Layout::COMPOSER_MAX_HEIGHT))
    }
}

impl<E: Styled + Sized> FxStyled for E {}

/// Builders for the partial styles that `.hover(...)` and `.active(...)` take.
///
/// GPUI 0.2.2 hands a hover callback a `StyleRefinement`, whose fields are all
/// `Option` and whose `text` sub-style is itself optional. These helpers keep
/// that shape out of component code, so the GPUI-version-specific details live
/// in exactly one place.
pub trait FxRefinement {
    fn fx_background(self, color: Rgba) -> StyleRefinement;
    fn fx_text_color(self, color: Rgba) -> StyleRefinement;
    fn fx_border_color(self, color: Rgba) -> StyleRefinement;
    fn fx_opacity(self, opacity: f32) -> StyleRefinement;
    /// Replace the text color only, leaving any other text style alone.
    fn fx_retint_text(self, color: Rgba) -> StyleRefinement;
}

impl FxRefinement for StyleRefinement {
    fn fx_background(mut self, color: Rgba) -> StyleRefinement {
        self.background = Some(Fill::Color(color.into()));
        self
    }

    fn fx_text_color(self, color: Rgba) -> StyleRefinement {
        self.fx_retint_text(color)
    }

    fn fx_retint_text(mut self, color: Rgba) -> StyleRefinement {
        let mut text = self.text.unwrap_or_default();
        text.color = Some(color.into());
        self.text = Some(text);
        self
    }

    fn fx_border_color(mut self, color: Rgba) -> StyleRefinement {
        self.border_color = Some(color.into());
        self
    }

    fn fx_opacity(mut self, opacity: f32) -> StyleRefinement {
        self.opacity = Some(opacity);
        self
    }
}

/// Where a dropdown or context menu should appear relative to its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Prefer below the anchor, flipping above when it would not fit.
    Automatic,
    Above,
    Below,
}

/// Which horizontal edge of the anchor a menu lines up with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    Leading,
    Trailing,
}

/// Vertical gap between an anchor and the panel it opens.
pub const MENU_ANCHOR_GAP: f32 = Layout::MENU_ANCHOR_GAP;
/// Margin between a menu panel and the window edge it is clamped to.
pub const MENU_EDGE_MARGIN: f32 = Layout::MENU_EDGE_MARGIN;
/// Minimum panel width, even when the label is narrower.
pub const MENU_MINIMUM_WIDTH: f32 = 160.0;

/// Resolve a menu panel's origin.
///
/// `anchor` is the anchor's origin in window coordinates, `anchor_size` and
/// `panel` are measured sizes, and `visible` is the window's drawable area.
/// Pure geometry, so it is unit-testable without a window.
pub fn resolve_menu_position(
    anchor: gpui::Point<Pixels>,
    anchor_size: gpui::Size<Pixels>,
    panel: gpui::Size<Pixels>,
    visible: gpui::Size<Pixels>,
    placement: Placement,
    alignment: Alignment,
) -> gpui::Point<Pixels> {
    let anchor_min_x = f64::from(anchor.x);
    let anchor_max_x = anchor_min_x + f64::from(anchor_size.width);
    let anchor_min_y = f64::from(anchor.y);
    let anchor_max_y = anchor_min_y + f64::from(anchor_size.height);

    let panel_w = f64::from(panel.width);
    let panel_h = f64::from(panel.height);
    let visible_w = f64::from(visible.width);
    let visible_h = f64::from(visible.height);

    let m = f64::from(MENU_EDGE_MARGIN);
    let g = f64::from(MENU_ANCHOR_GAP);

    // Horizontal: align to the chosen edge, then clamp inside the window.
    let x = match alignment {
        Alignment::Leading => anchor_min_x,
        Alignment::Trailing => anchor_max_x - panel_w,
    }
    .clamp(m, (visible_w - panel_w - m).max(m));

    // Vertical: `below` puts the panel's top edge under the anchor's bottom
    // edge. `automatic` prefers below and flips only if it would not fit.
    let below_y = anchor_max_y + g;
    let above_y = anchor_min_y - g - panel_h;
    let y = match placement {
        Placement::Below => below_y,
        Placement::Above => above_y,
        Placement::Automatic => {
            if below_y + panel_h <= visible_h - m {
                below_y
            } else {
                above_y
            }
        }
    }
    .clamp(m, (visible_h - panel_h - m).max(m));

    gpui::point(gpui::px(x as f32), gpui::px(y as f32))
}

/// Panel width for a menu: the requested width, the anchor's width, or a floor.
pub fn menu_panel_width(requested: Option<f32>, anchor_width: f32) -> f32 {
    requested
        .unwrap_or(0.0)
        .max(anchor_width)
        .max(MENU_MINIMUM_WIDTH)
}

/// Composite a translucent token over an opaque one.
///
/// GPUI composites translucent fills at draw time, so this is only needed where
/// a *measurement* must match what renders — contrast checks, chiefly.
pub fn flatten(foreground: Rgba, background: Rgba) -> Hsla {
    let a = foreground.a;
    let r = foreground.r * a + background.r * (1.0 - a);
    let g = foreground.g * a + background.g * (1.0 - a);
    let b = foreground.b * a + background.b * (1.0 - a);
    Hsla::from(Rgba { r, g, b, a: 1.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_refinement_carries_the_color() {
        let style = StyleRefinement::default().fx_background(crate::theme::h(0x112233));
        let Some(Fill::Color(c)) = style.background else {
            panic!("expected a solid fill");
        };
        // `Background` deliberately does not expose its channels, so the
        // assertion is that a solid fill was stored at all.
        assert!(!c.is_transparent(), "fill lost its opacity");
    }

    #[test]
    fn a_text_refinement_is_created_on_demand() {
        // `text` starts as None; asking for a color must create it.
        let style = StyleRefinement::default().fx_text_color(crate::theme::h(0xffffff));
        let text = style.text.expect("text refinement was not created");
        assert!(text.color.is_some());
    }

    #[test]
    fn retinting_preserves_the_other_text_fields() {
        let mut base = StyleRefinement::default().fx_text_color(crate::theme::h(0xffffff));
        base.text.as_mut().unwrap().font_weight = Some(gpui::FontWeight(600.0));

        let retinted = base.clone().fx_retint_text(crate::theme::h(0x000000));
        assert_eq!(
            retinted.text.unwrap().font_weight,
            Some(gpui::FontWeight(600.0)),
            "retinting dropped an unrelated text field"
        );
    }

    #[test]
    fn menu_prefers_below_when_there_is_room() {
        let at = gpui::point(gpui::px(100.0), gpui::px(100.0));
        let size = gpui::size(gpui::px(220.0), gpui::px(30.0));
        let panel = gpui::size(gpui::px(220.0), gpui::px(200.0));
        let visible = gpui::size(gpui::px(1000.0), gpui::px(800.0));
        let pos = resolve_menu_position(
            at,
            size,
            panel,
            visible,
            Placement::Automatic,
            Alignment::Leading,
        );
        // Below: anchor bottom (130) + gap (8).
        assert!(
            (f64::from(pos.y) - 138.0).abs() < 0.01,
            "y = {}",
            f64::from(pos.y)
        );
    }

    #[test]
    fn menu_flips_and_clamps_when_it_would_overflow_the_bottom() {
        let at = gpui::point(gpui::px(100.0), gpui::px(700.0));
        let size = gpui::size(gpui::px(220.0), gpui::px(30.0));
        let panel = gpui::size(gpui::px(220.0), gpui::px(400.0));
        let visible = gpui::size(gpui::px(1000.0), gpui::px(800.0));
        let pos = resolve_menu_position(
            at,
            size,
            panel,
            visible,
            Placement::Automatic,
            Alignment::Leading,
        );
        assert!(
            f64::from(pos.y) + 400.0 <= 800.0 - 8.0 + 0.01,
            "y = {} overflowed",
            f64::from(pos.y)
        );
    }

    #[test]
    fn trailing_alignment_right_edges_line_up() {
        let at = gpui::point(gpui::px(100.0), gpui::px(100.0));
        let size = gpui::size(gpui::px(200.0), gpui::px(30.0));
        let panel = gpui::size(gpui::px(220.0), gpui::px(200.0));
        let visible = gpui::size(gpui::px(1000.0), gpui::px(800.0));
        let pos = resolve_menu_position(
            at,
            size,
            panel,
            visible,
            Placement::Below,
            Alignment::Trailing,
        );
        // Anchor right edge (300) minus panel width (220).
        assert!(
            (f64::from(pos.x) - 80.0).abs() < 0.01,
            "x = {}",
            f64::from(pos.x)
        );
    }

    #[test]
    fn menu_never_escapes_the_left_edge() {
        let at = gpui::point(gpui::px(2.0), gpui::px(100.0));
        let size = gpui::size(gpui::px(50.0), gpui::px(30.0));
        let panel = gpui::size(gpui::px(220.0), gpui::px(200.0));
        let visible = gpui::size(gpui::px(1000.0), gpui::px(800.0));
        let pos = resolve_menu_position(
            at,
            size,
            panel,
            visible,
            Placement::Below,
            Alignment::Leading,
        );
        assert!(
            f64::from(pos.x) >= MENU_EDGE_MARGIN as f64 - 0.01,
            "x = {}",
            f64::from(pos.x)
        );
    }

    #[test]
    fn panel_width_never_drops_below_the_floor() {
        assert_eq!(menu_panel_width(None, 40.0), MENU_MINIMUM_WIDTH);
        assert_eq!(menu_panel_width(Some(300.0), 40.0), 300.0);
        assert_eq!(menu_panel_width(Some(100.0), 250.0), 250.0);
    }

    #[test]
    fn the_default_menu_width_leaves_room_for_a_title_beside_the_gutter() {
        // The real risk is not the constant but the relationship: if the check
        // gutter ever grew to meet the panel width, no title could be shown.
        let width = menu_panel_width(None, 0.0);
        assert!(
            width > Layout::MENU_CHECK_GUTTER,
            "a {width}pt panel cannot show a title beside a {}pt gutter",
            Layout::MENU_CHECK_GUTTER
        );
    }

    #[test]
    fn the_menu_geometry_comes_from_the_token_layer() {
        // A view that hardcoded its own copy of these would drift from the
        // tokens, so they are read back from the same place the view reads.
        assert_eq!(MENU_ANCHOR_GAP, Layout::MENU_ANCHOR_GAP);
        assert_eq!(MENU_EDGE_MARGIN, Layout::MENU_EDGE_MARGIN);
    }

    #[test]
    fn flattening_a_translucent_token_yields_the_composited_opaque_color() {
        let fg = Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 0.5,
        };
        let bg = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        // Half-strength red over black is rgb(0.5, 0, 0).
        let out = flatten(fg, bg).to_rgb();
        assert!((out.r - 0.5).abs() < 1e-3, "red was {}", out.r);
        assert!(out.g.abs() < 1e-3, "green was {}", out.g);
        assert!(out.b.abs() < 1e-3, "blue was {}", out.b);
        assert_eq!(out.a, 1.0, "flattened color must be opaque");
    }
}

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, CursorStyle, IntoElement, ParentElement, SharedString, StyleRefinement,
    Window, div,
};

use crate::icon::Icon;
use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{
    BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, WithAlpha, global,
};

/// Visual weight of a labelled button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonStyle {
    /// Solid accent fill. One per screen at most.
    #[default]
    Primary,
    /// Surface fill with a hairline edge. The default for most actions.
    Secondary,
    /// No chrome until hovered.
    Ghost,
    /// Destructive action.
    Danger,
}

/// Opacity applied to a disabled control.
const DISABLED_OPACITY: f32 = 0.52;
/// Opacity applied while a button is held down.
///
/// The original scaled the control to 0.97. GPUI's style refinement cannot
/// transform an element, so press feedback is an opacity dip instead — which
/// also avoids re-running layout on every press.
const PRESSED_OPACITY: f32 = 0.94;

/// How a button looks, as a function of style and hover.
///
/// Split out from rendering so the color decisions can be asserted directly,
/// without standing up a window.
#[derive(Debug, Clone, Copy)]
pub struct ButtonPalette {
    pub foreground: gpui::Rgba,
    pub background: gpui::Rgba,
    pub border: gpui::Rgba,
    /// An extra wash laid over the background, used by the danger hover.
    pub hover_tint: gpui::Rgba,
}

pub fn button_palette(
    style: ButtonStyle,
    hovered: bool,
    theme: &crate::theme::Theme,
) -> ButtonPalette {
    use gpui::Rgba;
    let clear = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };
    match style {
        ButtonStyle::Primary => ButtonPalette {
            foreground: Colors::on_accent(theme),
            background: if hovered {
                Colors::accent_hover(theme)
            } else {
                Colors::accent(theme)
            },
            border: clear,
            hover_tint: clear,
        },
        ButtonStyle::Secondary => ButtonPalette {
            foreground: Colors::fg(theme),
            background: if hovered {
                Colors::bg_hover(theme)
            } else {
                Colors::bg_surface(theme)
            },
            border: Colors::border(theme),
            hover_tint: clear,
        },
        ButtonStyle::Ghost => ButtonPalette {
            foreground: if hovered {
                Colors::fg(theme)
            } else {
                Colors::fg_secondary(theme)
            },
            background: if hovered {
                Colors::bg_hover(theme)
            } else {
                clear
            },
            border: clear,
            hover_tint: clear,
        },
        ButtonStyle::Danger => ButtonPalette {
            foreground: Colors::error(theme),
            // The tint deepens on hover by layering the muted token a second
            // time, rather than introducing a hand-tuned second opacity.
            background: Colors::error_muted(theme),
            border: Colors::error(theme).with_alpha(0.3),
            hover_tint: if hovered {
                Colors::error_muted(theme)
            } else {
                clear
            },
        },
    }
}

/// A labelled button, optionally with a leading glyph.
pub fn button(
    label: impl Into<SharedString>,
    style: ButtonStyle,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    button_with_icon(label, None, style, enabled, on_click)
}

/// A labelled button with a leading glyph.
pub fn button_with_icon(
    label: impl Into<SharedString>,
    icon: Option<Icon>,
    style: ButtonStyle,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = global::current();
    let label = label.into();
    let button_id: SharedString = format!("button:{label}").into();
    let palette = button_palette(style, false, &theme);
    let icon_element = icon.map(|i| i.render(IconSize::Control, palette.foreground, &theme));

    let mut content = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .min_h(px_len(Layout::CONTROL_HEIGHT))
        .gap(px_len(Spacing::XS))
        .debug_selector(|| "button-content".into());
    if let Some(el) = icon_element {
        content = content.child(
            div()
                .flex()
                .flex_shrink_0()
                .debug_selector(|| "button-icon".into())
                .child(el),
        );
    }
    content = content
        .child(
            div()
                .whitespace_nowrap()
                .fx_role(TextRole::BodyMedium)
                .debug_selector(|| "button-label".into())
                .child(label),
        )
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::XXS))
        .rounded(px_len(Radii::SM))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(palette.border)
        .bg(palette.background)
        .text_color(palette.foreground);

    content
        .opacity(if enabled { 1.0 } else { DISABLED_OPACITY })
        .id(button_id)
        .hover(|s: StyleRefinement| {
            if !enabled {
                return s;
            }
            let p = button_palette(style, true, &theme);
            s.fx_background(p.background).fx_text_color(p.foreground)
        })
        .active(|s: StyleRefinement| {
            if enabled {
                s.fx_opacity(PRESSED_OPACITY)
            } else {
                s
            }
        })
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::default()
        })
        .on_click(move |event, window, cx| {
            if enabled {
                on_click(event, window, cx);
            }
        })
}

/// How much chrome an icon button shows at rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconButtonVariant {
    /// A subtle surface fill; used in pane headers.
    #[default]
    Filled,
    /// No chrome at rest, highlight on hover. Selection tints the glyph with
    /// the accent. Used in the window title bar.
    Ghost,
}

/// Default edge length for an icon button.
pub const ICON_BUTTON_SIZE: f32 = Layout::CONTROL_HEIGHT;
/// Compact toolbar size.
const SMALL_ICON_BUTTON_SIZE: f32 = Layout::CONTROL_COMPACT_HEIGHT;
/// Row-trailing controls keep a compact target with a smaller glyph.
const TINY_ICON_BUTTON_SIZE: f32 = Layout::CONTROL_COMPACT_HEIGHT;

/// The colors an icon button resolves to.
#[derive(Debug, Clone, Copy)]
pub struct IconButtonPalette {
    pub foreground: gpui::Rgba,
    pub background: gpui::Rgba,
    pub border: gpui::Rgba,
}

pub fn icon_button_palette(
    variant: IconButtonVariant,
    selected: bool,
    hovered: bool,
    enabled: bool,
    tint: Option<gpui::Rgba>,
    theme: &crate::theme::Theme,
) -> IconButtonPalette {
    use gpui::Rgba;
    let clear = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    let foreground = if !enabled {
        Colors::fg_quaternary(theme)
    } else if let Some(tint) = tint {
        tint
    } else if variant == IconButtonVariant::Ghost {
        if selected {
            Colors::accent(theme)
        } else if hovered {
            Colors::fg(theme)
        } else {
            Colors::fg_tertiary(theme)
        }
    } else if selected || hovered {
        Colors::fg(theme)
    } else {
        Colors::fg_tertiary(theme)
    };

    let background = if variant == IconButtonVariant::Ghost {
        if enabled && hovered {
            Colors::bg_hover(theme)
        } else {
            clear
        }
    } else if !enabled {
        Colors::bg_surface(theme).with_alpha(0.3)
    } else if selected {
        Colors::bg_selected(theme)
    } else if hovered {
        Colors::bg_hover(theme)
    } else {
        Colors::bg_surface(theme).with_alpha(0.55)
    };

    let border = if variant == IconButtonVariant::Ghost {
        clear
    } else if selected {
        Colors::border_medium(theme)
    } else if hovered {
        Colors::border_subtle(theme)
    } else {
        clear
    };

    IconButtonPalette {
        foreground,
        background,
        border,
    }
}

/// Everything that varies about an icon button.
///
/// Grouped into a struct because the parameter list was nine positional
/// arguments, at which point a caller has to count to get it right.
#[derive(Debug, Clone, PartialEq)]
pub struct IconButtonProps {
    pub icon: Icon,
    /// Accessible label and tooltip text.
    pub label: SharedString,
    pub selected: bool,
    /// Overrides the state-derived foreground.
    pub tint: Option<gpui::Rgba>,
    pub enabled: bool,
    pub variant: IconButtonVariant,
    pub size: f32,
    pub icon_size: IconSize,
}

impl IconButtonProps {
    pub fn new(icon: Icon, label: impl Into<SharedString>) -> Self {
        Self {
            icon,
            label: label.into(),
            selected: false,
            tint: None,
            enabled: true,
            variant: IconButtonVariant::default(),
            size: ICON_BUTTON_SIZE,
            icon_size: IconSize::Control,
        }
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn tint(mut self, tint: gpui::Rgba) -> Self {
        self.tint = Some(tint);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn variant(mut self, variant: IconButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn icon_size(mut self, size: IconSize) -> Self {
        self.icon_size = size;
        self
    }

    /// The compact size, for a toolbar beside text.
    pub fn small(mut self) -> Self {
        self.size = SMALL_ICON_BUTTON_SIZE;
        self.icon_size = IconSize::Control;
        self
    }

    /// The row-trailing size, for a disclosure or per-row action.
    ///
    /// Keeps the compact hit target with a smaller glyph, so a trailing action
    /// does not compete with the row label.
    pub fn tiny(mut self) -> Self {
        self.size = TINY_ICON_BUTTON_SIZE;
        self.icon_size = IconSize::Small;
        self
    }

    /// No resting fill, so the control borrows its row's background.
    pub fn ghost(mut self) -> Self {
        self.variant = IconButtonVariant::Ghost;
        self
    }

    /// Derive a stable element id, so two buttons with the same label in
    /// different regions do not collide.
    fn element_id(&self) -> SharedString {
        format!("icon-button:{}:{}", self.icon.name(), self.label).into()
    }
}

/// Hit target for a row-trailing icon button.
///
/// The whole minimum-sized control owns click handling, including its padding.
const TINY_ICON_BUTTON_HIT: f32 = Layout::CONTROL_COMPACT_HEIGHT;

/// A square icon-only control.
pub fn icon_button(
    props: IconButtonProps,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = global::current();
    // Resolve the id before destructuring, since it reads the whole struct.
    let button_id = props.element_id();
    let IconButtonProps {
        icon,
        label,
        selected,
        tint,
        enabled,
        variant,
        size,
        icon_size,
    } = props;

    let palette = icon_button_palette(variant, selected, false, enabled, tint, &theme);
    let debug_label = format!("icon-button:{label}");
    let corner = if variant == IconButtonVariant::Ghost {
        Radii::SM
    } else {
        Radii::XS
    };

    let control = div()
        .debug_selector(move || debug_label.clone())
        .flex()
        .size(px_len(size.max(TINY_ICON_BUTTON_HIT)))
        .flex_shrink_0()
        .flex_col()
        .items_center()
        .justify_center()
        .rounded(px_len(corner))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(palette.border)
        .bg(palette.background)
        .child(icon.render(icon_size, palette.foreground, &theme))
        // `.id` upgrades the element to `Stateful<Div>`, which is what carries
        // `on_click` and `tooltip` in GPUI 0.2.2.
        .id(button_id)
        .hover(|s: StyleRefinement| {
            let p = icon_button_palette(variant, selected, true, enabled, tint, &theme);
            s.fx_background(p.background).fx_border_color(p.border)
        })
        .active(|s| {
            if enabled {
                s.fx_opacity(PRESSED_OPACITY)
            } else {
                s
            }
        })
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::default()
        })
        .tooltip(move |window, cx| crate::components::tooltip::tooltip(label.clone(), window, cx))
        .on_click(move |event, window, cx| {
            if enabled {
                on_click(event, window, cx);
            }
        });

    control.into_any_element()
}

/// A dense button for panel toolbars, where vertical space is scarce.
pub fn compact_action_button(
    label: impl Into<SharedString>,
    icon: Option<Icon>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = global::current();
    let label = label.into();
    let button_id: SharedString = format!("compact-action:{label}").into();

    let mut content = div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap(px_len(Spacing::XXS));
    if let Some(i) = icon {
        content = content.child(i.render(IconSize::Micro, Colors::fg_secondary(&theme), &theme));
    }
    content = content
        .child(
            div()
                .whitespace_nowrap()
                .fx_role(TextRole::CaptionMedium)
                .child(label),
        )
        .px(px_len(Spacing::SM))
        .min_h(px_len(Layout::CONTROL_COMPACT_HEIGHT))
        .rounded(px_len(Radii::SM))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .bg(Colors::bg_elevated(&theme))
        .text_color(Colors::fg_secondary(&theme));

    content
        .id(button_id)
        .hover(|s: StyleRefinement| {
            s.fx_background(Colors::bg_hover(&theme))
                .fx_text_color(Colors::fg(&theme))
        })
        .active(|s| s.fx_opacity(PRESSED_OPACITY))
        .cursor(CursorStyle::PointingHand)
        .on_click(on_click)
}

/// A selectable answer or preference, with enough room for its explanation.
/// Unlike toolbar buttons, both labels wrap within the available panel width.
pub fn choice_row(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    selected: bool,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = global::current();
    let label = label.into();
    let description = description.into();
    let mut labels = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px_len(Spacing::XXS))
        .child(
            div()
                .fx_role(TextRole::BodyMedium)
                .text_color(Colors::fg(&theme))
                .child(label.clone()),
        );
    if !description.is_empty() {
        labels = labels.child(
            div()
                .fx_role(TextRole::Caption)
                .text_color(Colors::fg_secondary(&theme))
                .child(description),
        );
    }
    div()
        .id(SharedString::from(format!("choice:{label}")))
        .w_full()
        .min_w_0()
        .flex()
        .items_start()
        .gap(px_len(Spacing::SM))
        .p(px_len(Spacing::MD))
        .rounded(px_len(Radii::SM))
        .border(px_len(BorderWidth::REGULAR))
        .border_color(if selected {
            Colors::accent(&theme)
        } else {
            Colors::border_subtle(&theme)
        })
        .bg(if selected {
            Colors::bg_selected(&theme)
        } else {
            Colors::bg_elevated(&theme)
        })
        .opacity(if enabled { 1.0 } else { DISABLED_OPACITY })
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::default()
        })
        .hover(move |s| {
            if enabled && !selected {
                s.fx_background(Colors::bg_hover(&theme))
            } else {
                s
            }
        })
        .active(move |s| {
            if enabled {
                s.fx_opacity(PRESSED_OPACITY)
            } else {
                s
            }
        })
        .on_click(move |event, window, cx| {
            if enabled {
                on_click(event, window, cx)
            }
        })
        .child(
            div()
                .flex_shrink_0()
                .pt(px_len(Spacing::XXXS))
                .child(if selected {
                    Icon::CHECKMARK_CIRCLE_FILL.render(
                        IconSize::Medium,
                        Colors::accent(&theme),
                        &theme,
                    )
                } else {
                    div()
                        .size(px_len(
                            crate::theme::Typography::icon(IconSize::Medium, &theme).size,
                        ))
                        .rounded_full()
                        .border(px_len(BorderWidth::REGULAR))
                        .border_color(Colors::border_medium(&theme))
                        .into_any_element()
                }),
        )
        .child(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{AccentColor, AppearanceMode, BaseTone, Theme};

    fn themes() -> Vec<Theme> {
        let mut out = Vec::new();
        for tone in BaseTone::ALL {
            for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
                out.push(Theme {
                    base_tone: tone,
                    appearance_mode: mode,
                    ..Default::default()
                });
            }
        }
        out
    }

    #[test]
    fn every_button_style_has_a_distinct_resting_background() {
        let theme = Theme::default();
        let seen: Vec<_> = [
            ButtonStyle::Primary,
            ButtonStyle::Secondary,
            ButtonStyle::Ghost,
            ButtonStyle::Danger,
        ]
        .into_iter()
        .map(|s| button_palette(s, false, &theme).background)
        .collect();
        for (i, a) in seen.iter().enumerate() {
            for b in &seen[i + 1..] {
                assert_ne!(a, b, "two button styles share a resting background");
            }
        }
    }

    /// How a button looks once its background and hover tint are combined.
    ///
    /// An absent tint means "no change", so it is skipped rather than
    /// composited: `composite` deliberately returns an opaque color, which
    /// would flatten a transparent resting fill to opaque black.
    fn effective_fill(p: ButtonPalette, _theme: &Theme) -> gpui::Rgba {
        if p.hover_tint.a == 0.0 {
            p.background
        } else {
            crate::theme::composite(p.hover_tint, p.background)
        }
    }

    #[test]
    fn a_hovered_button_never_matches_its_resting_appearance() {
        let theme = Theme::default();
        for style in [
            ButtonStyle::Primary,
            ButtonStyle::Secondary,
            ButtonStyle::Ghost,
            ButtonStyle::Danger,
        ] {
            let rest = button_palette(style, false, &theme);
            let hover = button_palette(style, true, &theme);
            assert_ne!(
                effective_fill(rest, &theme),
                effective_fill(hover, &theme),
                "{style:?} gives no hover feedback"
            );
        }
    }

    #[test]
    fn every_theme_gives_every_button_style_hover_feedback() {
        for tone in BaseTone::ALL {
            for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
                let theme = Theme {
                    base_tone: tone,
                    appearance_mode: mode,
                    ..Default::default()
                };
                for style in [
                    ButtonStyle::Primary,
                    ButtonStyle::Secondary,
                    ButtonStyle::Ghost,
                    ButtonStyle::Danger,
                ] {
                    let rest = button_palette(style, false, &theme);
                    let hover = button_palette(style, true, &theme);
                    assert_ne!(
                        effective_fill(rest, &theme),
                        effective_fill(hover, &theme),
                        "{style:?} is inert in {tone:?} {mode:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn ghost_and_primary_carry_no_border() {
        let theme = Theme::default();
        for style in [ButtonStyle::Ghost, ButtonStyle::Primary] {
            assert_eq!(
                button_palette(style, false, &theme).border.a,
                0.0,
                "{style:?} should have no border"
            );
        }
        assert!(
            button_palette(ButtonStyle::Secondary, false, &theme)
                .border
                .a
                > 0.0
        );
    }

    #[test]
    fn the_danger_hover_layers_a_second_tint() {
        let theme = Theme::default();
        let rest = button_palette(ButtonStyle::Danger, false, &theme);
        let hover = button_palette(ButtonStyle::Danger, true, &theme);
        assert_eq!(rest.hover_tint.a, 0.0);
        assert!(hover.hover_tint.a > 0.0, "danger has no hover depth");
    }

    #[test]
    fn primary_foreground_stays_legible_on_its_fill_in_every_theme() {
        for theme in themes() {
            for accent in AccentColor::ALL {
                let theme = Theme {
                    accent_color: accent,
                    ..theme
                };
                let p = button_palette(ButtonStyle::Primary, false, &theme);
                let ratio = crate::theme::contrast(p.foreground, p.background);
                assert!(
                    ratio >= 3.0,
                    "{accent:?} {}: primary label is {ratio}:1",
                    theme.appearance_mode
                );
            }
        }
    }

    #[test]
    fn a_disabled_icon_button_recedes() {
        let theme = Theme::default();
        let enabled =
            icon_button_palette(IconButtonVariant::Filled, false, false, true, None, &theme);
        let disabled =
            icon_button_palette(IconButtonVariant::Filled, false, false, false, None, &theme);
        assert_ne!(enabled.foreground, disabled.foreground);
        assert_ne!(enabled.background, disabled.background);
    }

    #[test]
    fn a_ghost_icon_button_has_no_chrome_at_rest() {
        let theme = Theme::default();
        let p = icon_button_palette(IconButtonVariant::Ghost, false, false, true, None, &theme);
        assert_eq!(p.background.a, 0.0);
        assert_eq!(p.border.a, 0.0);
    }

    #[test]
    fn a_ghost_icon_button_gains_its_border_only_on_hover() {
        let theme = Theme::default();
        let rest = icon_button_palette(IconButtonVariant::Filled, false, false, true, None, &theme);
        let hover = icon_button_palette(IconButtonVariant::Filled, false, true, true, None, &theme);
        assert_eq!(rest.border.a, 0.0, "resting border should be invisible");
        assert!(hover.border.a > 0.0);
    }

    #[test]
    fn an_explicit_tint_beats_every_state() {
        let theme = Theme::default();
        let tint = crate::theme::Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        for variant in [IconButtonVariant::Filled, IconButtonVariant::Ghost] {
            for selected in [true, false] {
                for hovered in [true, false] {
                    let p =
                        icon_button_palette(variant, selected, hovered, true, Some(tint), &theme);
                    assert_eq!(p.foreground, tint, "tint was overridden");
                }
            }
        }
    }

    #[test]
    fn selection_tints_a_ghost_glyph_with_the_accent() {
        let theme = Theme::default();
        let selected =
            icon_button_palette(IconButtonVariant::Ghost, true, false, true, None, &theme);
        assert_eq!(selected.foreground, Colors::accent(&theme));
    }
}

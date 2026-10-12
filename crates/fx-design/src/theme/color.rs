//! The FlowX color system.
//!
//! This mirrors the three-layer architecture of the original Swift design system,
//! which behaves like CSS custom properties:
//!
//! ```text
//! Colors::fg(&theme)          semantic tokens read by every view
//!        ↓
//! Theme { base_tone, appearance, accent, text_size }
//!        ↓
//! Palette::get(tone, dark)    the "CSS variables" backing layer
//!        ↓
//! ToneScale                   11 shades per tone, 50 → 950
//! ```
//!
//! Rules that survive the port and must keep holding:
//!
//! - Views never hardcode a color. If a token is missing, add it here first.
//! - `fg`, `fgSecondary`, and `fgTertiary` clear WCAG AA on `bg` and `bgElevated`
//!   in every tone and appearance, and stay in that order.
//! - Diffs use the dedicated `diff*` tokens, never a semantic color at low opacity.

use serde::{Deserialize, Serialize};

// Re-exported so every crate that reads a token can name the type without
// taking a direct dependency on GPUI.
pub use gpui::Rgba;

/// Build an opaque color from a `0xRRGGBB` literal.
#[inline]
pub const fn h(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

/// Build a translucent color from a `0xRRGGBB` literal and an alpha.
#[inline]
pub const fn ha(hex: u32, alpha: f32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: alpha,
    }
}

/// Replace a color's alpha, leaving its channels alone.
///
/// GPUI names this `alpha`, which reads like a getter at a call site such as
/// `Colors::error(theme).alpha(0.3)`.
pub trait WithAlpha {
    fn with_alpha(self, alpha: f32) -> Rgba;
}

impl WithAlpha for Rgba {
    fn with_alpha(self, alpha: f32) -> Rgba {
        Rgba { a: alpha, ..self }
    }
}

/// The base neutral ramp. Each is Tailwind-derived, 11 steps from lightest (50)
/// to darkest (950).
///
/// Dark mode reads top-down (900 = background, 800 = elevated, 700 = surface);
/// light mode reads bottom-up (50 = background, 100 = elevated, 200 = surface).
/// The same scale serves both, so a tone stays cohesive across appearances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToneScale {
    hex: [u32; 11],
}

impl ToneScale {
    /// Shade at array position `i`, where 0 is 50 and 10 is 950.
    #[inline]
    pub const fn shade(&self, i: usize) -> Rgba {
        h(self.hex[i])
    }

    /// Interpolate between two shade positions.
    ///
    /// Used only by tokens that must clear a contrast floor the nearest
    /// supplied stop just misses — see `fg_tertiary` in light mode.
    /// `amount` of 0.0 yields `from`, 1.0 yields `to`.
    pub fn mix(&self, from: usize, to: usize, amount: f64) -> Rgba {
        let a = self.hex[from];
        let b = self.hex[to];
        let channel = |shift: u32| -> u32 {
            let lhs = ((a >> shift) & 0xFF) as f64;
            let rhs = ((b >> shift) & 0xFF) as f64;
            (lhs + (rhs - lhs) * amount).round() as u32
        };
        h((channel(16) << 16) | (channel(8) << 8) | channel(0))
    }

    /// Slate — desaturated cool gray with a barely perceptible cool undertone.
    pub const SLATE: ToneScale = ToneScale {
        hex: [
            0xF9F9FA, 0xF3F3F4, 0xE4E5E7, 0xD1D3D6, 0x989BA1, 0x6F7279, 0x50545B, 0x3C4047,
            0x24272C, 0x16181D, 0x0A0B0E,
        ],
    };

    /// Zinc — near-neutral, the FlowX default.
    pub const ZINC: ToneScale = ToneScale {
        hex: [
            0xFAFAFA, 0xF4F4F5, 0xE4E4E7, 0xD4D4D8, 0x9F9FA9, 0x71717B, 0x52525C, 0x3F3F46,
            0x27272A, 0x18181B, 0x09090B,
        ],
    };

    /// Neutral — pure gray.
    pub const NEUTRAL: ToneScale = ToneScale {
        hex: [
            0xFAFAFA, 0xF5F5F5, 0xE5E5E5, 0xD4D4D4, 0xA1A1A1, 0x737373, 0x525252, 0x404040,
            0x262626, 0x171717, 0x0A0A0A,
        ],
    };

    /// Stone — warm gray.
    pub const STONE: ToneScale = ToneScale {
        hex: [
            0xFAFAF9, 0xF5F5F4, 0xE7E5E4, 0xD6D3D1, 0xA6A09B, 0x79716B, 0x57534D, 0x44403B,
            0x292524, 0x1C1917, 0x0C0A09,
        ],
    };

    pub const fn for_tone(tone: BaseTone) -> ToneScale {
        match tone {
            BaseTone::Slate => Self::SLATE,
            BaseTone::Zinc => Self::ZINC,
            BaseTone::Neutral => Self::NEUTRAL,
            BaseTone::Stone => Self::STONE,
        }
    }
}

/// Shade index for each step, so token code reads as `shade::BG` rather than a
/// bare magic number.
pub mod shade {
    pub const P50: usize = 0;
    pub const P100: usize = 1;
    pub const P200: usize = 2;
    pub const P300: usize = 3;
    pub const P400: usize = 4;
    pub const P500: usize = 5;
    pub const P600: usize = 6;
    pub const P700: usize = 7;
    pub const P800: usize = 8;
    pub const P900: usize = 9;
    pub const P950: usize = 10;
}

/// The "CSS variables" backing layer, resolved from a tone and an appearance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    // Backgrounds
    pub bg: Rgba,
    pub bg_elevated: Rgba,
    pub bg_surface: Rgba,
    pub bg_hover: Rgba,
    pub bg_selected: Rgba,
    pub bg_pressed: Rgba,

    // Foregrounds
    pub fg: Rgba,
    pub fg_secondary: Rgba,
    pub fg_tertiary: Rgba,
    pub fg_quaternary: Rgba,

    // Borders
    pub border: Rgba,
    pub border_medium: Rgba,
    pub border_subtle: Rgba,

    // Overlays
    pub overlay: Rgba,
    pub overlay_light: Rgba,

    /// Terminal background. Deliberately translucent over `bg` in dark mode so
    /// the pane reads as slightly recessed rather than a flat block.
    pub terminal_bg: Rgba,

    /// Teal secondary accent; adapts per mode.
    pub accent_secondary: Rgba,
    pub accent_secondary_muted: Rgba,
    /// The base tone's darkest ink, used as `on_accent` for accents too light
    /// to carry white text.
    pub on_accent_dark: Rgba,

    // Semantic (mode-adapted)
    pub success: Rgba,
    pub warning: Rgba,
    pub error: Rgba,
    pub info: Rgba,
    pub success_muted: Rgba,
    pub warning_muted: Rgba,
    pub error_muted: Rgba,
    pub info_muted: Rgba,

    // Diff. Proper semantic pairs, not opacity-modified semantic colors.
    pub diff_added_bg: Rgba,
    pub diff_removed_bg: Rgba,
    pub diff_added_fg: Rgba,
    pub diff_removed_fg: Rgba,
    pub diff_context_bg: Rgba,

    /// Opaque background painted behind the window before any view draws.
    pub window_background: Rgba,
}

impl Palette {
    /// There are only eight tone/appearance combinations, so every palette is
    /// generated once at first use and reused for the life of the process.
    ///
    /// Indexed by `tone_index() * 2 + is_dark`, so lookup is two array
    /// subscripts rather than a hash.
    pub fn get(tone: BaseTone, dark: bool) -> &'static Palette {
        static PALETTES: std::sync::OnceLock<[Palette; 8]> = std::sync::OnceLock::new();
        let table = PALETTES.get_or_init(|| {
            std::array::from_fn(|i| {
                let tone = BaseTone::from_index(i / 2);
                Palette::generate(tone, i % 2 == 1)
            })
        });
        &table[tone.index() * 2 + usize::from(dark)]
    }

    pub fn generate(tone: BaseTone, dark: bool) -> Palette {
        let t = ToneScale::for_tone(tone);

        // Semantic hues are brighter on dark backgrounds, deeper on light ones.
        let success = if dark { h(0x34D399) } else { h(0x059669) };
        let warning = if dark { h(0xFBBF24) } else { h(0xD97706) };
        let error = if dark { h(0xF87171) } else { h(0xDC2626) };
        let info = if dark { h(0x60A5FA) } else { h(0x2563EB) };
        // The light value is teal-700 so it stays readable as text; #4ECDC4 is
        // only 1.76:1 on a light surface.
        let accent_secondary = if dark { h(0x4ECDC4) } else { h(0x0F766E) };
        // Dark surfaces absorb more tint, so they take slightly more alpha.
        let status_tint = if dark { 0.14 } else { 0.10 };
        let accent_tint = if dark { 0.16 } else { 0.12 };

        let tint = |c: Rgba| Rgba {
            a: status_tint,
            ..c
        };

        if dark {
            // 900=bg, 800=elevated, 700=surface, 50=fg, 300=fgSecondary, 400=fgTertiary
            Palette {
                bg: t.shade(shade::P900),
                bg_elevated: t.shade(shade::P800),
                bg_surface: t.shade(shade::P700),
                bg_hover: ha(0xFFFFFF, 0.04),
                bg_selected: ha(0xFFFFFF, 0.08),
                bg_pressed: ha(0xFFFFFF, 0.06),
                fg: t.shade(shade::P50),
                fg_secondary: t.shade(shade::P300),
                fg_tertiary: t.shade(shade::P400),
                fg_quaternary: ha(0xFFFFFF, 0.24),
                border: ha(t.hex[shade::P600], 0.6),
                border_medium: t.shade(shade::P600),
                border_subtle: ha(0xFFFFFF, 0.06),
                overlay: ha(0x000000, 0.5),
                overlay_light: ha(0x000000, 0.3),
                terminal_bg: Rgba {
                    a: 0.85,
                    ..t.shade(shade::P900)
                },
                accent_secondary,
                accent_secondary_muted: Rgba {
                    a: accent_tint,
                    ..accent_secondary
                },
                on_accent_dark: t.shade(shade::P950),
                success,
                warning,
                error,
                info,
                success_muted: tint(success),
                warning_muted: tint(warning),
                error_muted: tint(error),
                info_muted: tint(info),
                // Muted dark diff backgrounds, Codex/GitHub style.
                diff_added_bg: h(0x213A2B),
                diff_removed_bg: h(0x4A221D),
                diff_added_fg: h(0x34D399),
                diff_removed_fg: h(0xF87171),
                diff_context_bg: Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                },
                window_background: t.shade(shade::P800),
            }
        } else {
            // 50=bg, 100=elevated, 200=surface, 900=fg, 600=fgSecondary, ~500=fgTertiary
            Palette {
                bg: t.shade(shade::P50),
                bg_elevated: t.shade(shade::P100),
                bg_surface: t.shade(shade::P200),
                bg_hover: ha(0x000000, 0.04),
                bg_selected: ha(0x000000, 0.08),
                bg_pressed: ha(0x000000, 0.06),
                fg: t.shade(shade::P900),
                fg_secondary: t.shade(shade::P600),
                // 500 alone lands near 4.35:1 on elevated; a touch of 600 clears AA.
                fg_tertiary: t.mix(shade::P500, shade::P600, 0.15),
                fg_quaternary: ha(0x000000, 0.24),
                border: t.shade(shade::P300),
                // Must stay visible on bg_surface, e.g. dropdown and modal edges.
                border_medium: t.shade(shade::P300),
                border_subtle: ha(0x000000, 0.05),
                overlay: ha(0x000000, 0.18),
                overlay_light: ha(0x000000, 0.10),
                terminal_bg: t.shade(shade::P100),
                accent_secondary,
                accent_secondary_muted: Rgba {
                    a: accent_tint,
                    ..accent_secondary
                },
                on_accent_dark: t.shade(shade::P950),
                success,
                warning,
                error,
                info,
                success_muted: tint(success),
                warning_muted: tint(warning),
                error_muted: tint(error),
                info_muted: tint(info),
                // Pastel light diff backgrounds, GitHub style.
                diff_added_bg: h(0xDCFCE7),
                diff_removed_bg: h(0xFEE2E2),
                diff_added_fg: h(0x166534),
                diff_removed_fg: h(0x991B1B),
                diff_context_bg: Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                },
                window_background: t.shade(shade::P50),
            }
        }
    }
}

/// How the window decides between dark and light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AppearanceMode {
    /// Follow the OS. `Theme::system_appearance` carries the resolved value.
    #[default]
    System,
    Dark,
    Light,
}

impl std::fmt::Display for AppearanceMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl AppearanceMode {
    /// The next value in the set, wrapping at the end.
    ///
    /// Cycling has to live next to `ALL`, or the wrapping order and the declared
    /// order can drift apart and the wrap point becomes arbitrary.
    pub fn next(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + 1) % all.len()]
    }

    /// The previous value in the set, wrapping at the start.
    pub fn previous(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + all.len() - 1) % all.len()]
    }

    pub const ALL: [Self; 3] = [Self::System, Self::Dark, Self::Light];

    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }

    /// Whether this mode pins an appearance rather than deferring to the OS.
    pub const fn is_pinned(self) -> bool {
        !matches!(self, Self::System)
    }
}

/// The neutral ramp the whole app is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BaseTone {
    Slate,
    #[default]
    Zinc,
    Neutral,
    Stone,
}

impl std::fmt::Display for BaseTone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl BaseTone {
    /// The next value in the set, wrapping at the end.
    ///
    /// Cycling has to live next to `ALL`, or the wrapping order and the declared
    /// order can drift apart and the wrap point becomes arbitrary.
    pub fn next(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + 1) % all.len()]
    }

    /// The previous value in the set, wrapping at the start.
    pub fn previous(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + all.len() - 1) % all.len()]
    }

    pub const ALL: [Self; 4] = [Self::Slate, Self::Zinc, Self::Neutral, Self::Stone];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn from_index(i: usize) -> Self {
        Self::ALL[i]
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Slate => "Slate",
            Self::Zinc => "Zinc",
            Self::Neutral => "Neutral",
            Self::Stone => "Stone",
        }
    }

    /// One-line explanation shown next to the picker.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Slate => "Cool blue-gray",
            Self::Zinc => "Near-neutral",
            Self::Neutral => "Pure gray",
            Self::Stone => "Warm gray",
        }
    }
}

/// The accent, chosen independently of the base tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AccentColor {
    #[default]
    Violet,
    Blue,
    Emerald,
    Orange,
    Rose,
}

impl std::fmt::Display for AccentColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl AccentColor {
    /// The next value in the set, wrapping at the end.
    ///
    /// Cycling has to live next to `ALL`, or the wrapping order and the declared
    /// order can drift apart and the wrap point becomes arbitrary.
    pub fn next(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + 1) % all.len()]
    }

    /// The previous value in the set, wrapping at the start.
    pub fn previous(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + all.len() - 1) % all.len()]
    }

    pub const ALL: [Self; 5] = [
        Self::Violet,
        Self::Blue,
        Self::Emerald,
        Self::Orange,
        Self::Rose,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Violet => "Violet",
            Self::Blue => "Blue",
            Self::Emerald => "Emerald",
            Self::Orange => "Orange",
            Self::Rose => "Rose",
        }
    }

    pub const fn hex(self) -> u32 {
        match self {
            Self::Violet => 0x6C5CE7,
            Self::Blue => 0x4183F5,
            Self::Emerald => 0x1DB47A,
            Self::Orange => 0xED7A19,
            Self::Rose => 0xE2498B,
        }
    }

    pub const fn hover_hex(self) -> u32 {
        match self {
            Self::Violet => 0x7D6FF0,
            Self::Blue => 0x5A95F8,
            Self::Emerald => 0x2BC286,
            Self::Orange => 0xF78F2E,
            Self::Rose => 0xEE609E,
        }
    }

    pub const fn color(self) -> Rgba {
        h(self.hex())
    }

    pub const fn hover_color(self) -> Rgba {
        h(self.hover_hex())
    }

    /// Emerald and orange are too light for white labels (2.67:1 and 2.84:1), so
    /// solid fills in those accents carry the base tone's darkest ink instead.
    pub const fn prefers_dark_foreground(self) -> bool {
        matches!(self, Self::Emerald | Self::Orange)
    }
}

/// Global UI text density.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TextSizePreset {
    Compact,
    #[default]
    Standard,
    Comfortable,
    Large,
}

impl std::fmt::Display for TextSizePreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl TextSizePreset {
    /// The next value in the set, wrapping at the end.
    ///
    /// Cycling has to live next to `ALL`, or the wrapping order and the declared
    /// order can drift apart and the wrap point becomes arbitrary.
    pub fn next(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + 1) % all.len()]
    }

    /// The previous value in the set, wrapping at the start.
    pub fn previous(self) -> Self {
        let all = Self::ALL;
        let index = all.iter().position(|c| *c == self).unwrap_or(0);
        all[(index + all.len() - 1) % all.len()]
    }

    pub const ALL: [Self; 4] = [
        Self::Compact,
        Self::Standard,
        Self::Comfortable,
        Self::Large,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Standard => "Default",
            Self::Comfortable => "Comfortable",
            Self::Large => "Large",
        }
    }

    pub const fn scale(self) -> f32 {
        match self {
            Self::Compact => 0.93,
            Self::Standard => 1.0,
            Self::Comfortable => 1.08,
            Self::Large => 1.16,
        }
    }
}

/// Everything the token layer resolves from.
///
/// `Theme` is plain data with no GPUI dependency beyond `Rgba`, so token code
/// and its tests stay free of windowing concerns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub appearance_mode: AppearanceMode,
    pub base_tone: BaseTone,
    pub accent_color: AccentColor,
    pub text_size: TextSizePreset,
    /// The OS appearance, consulted only when `appearance_mode` is `System`.
    /// Defaults to dark so a headless test never depends on the desktop.
    pub system_appearance: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            appearance_mode: AppearanceMode::Dark,
            base_tone: BaseTone::Zinc,
            accent_color: AccentColor::Violet,
            text_size: TextSizePreset::Standard,
            system_appearance: true,
        }
    }
}

impl Theme {
    /// Replace one setting, leaving the rest alone.
    pub const fn with_appearance(self, appearance_mode: AppearanceMode) -> Self {
        Self {
            appearance_mode,
            ..self
        }
    }

    pub const fn with_tone(self, base_tone: BaseTone) -> Self {
        Self { base_tone, ..self }
    }

    pub const fn with_accent(self, accent_color: AccentColor) -> Self {
        Self {
            accent_color,
            ..self
        }
    }

    pub const fn with_text_size(self, text_size: TextSizePreset) -> Self {
        Self { text_size, ..self }
    }

    pub const fn new(
        appearance_mode: AppearanceMode,
        base_tone: BaseTone,
        accent_color: AccentColor,
        text_size: TextSizePreset,
    ) -> Self {
        Self {
            appearance_mode,
            base_tone,
            accent_color,
            text_size,
            system_appearance: true,
        }
    }

    /// Whether the app is currently rendering dark.
    pub const fn is_dark(&self) -> bool {
        match self.appearance_mode {
            AppearanceMode::Dark => true,
            AppearanceMode::Light => false,
            AppearanceMode::System => self.system_appearance,
        }
    }

    pub fn palette(&self) -> &'static Palette {
        Palette::get(self.base_tone, self.is_dark())
    }

    /// Multiplier applied to every typography size.
    pub const fn text_scale(&self) -> f32 {
        self.text_size.scale()
    }

    /// Whether anything resolved from these options changed.
    ///
    /// Views that cache token values across frames compare this to avoid
    /// re-running work when an unrelated state change triggers a re-render.
    pub fn signature(&self) -> ThemeSignature {
        ThemeSignature {
            base_tone: self.base_tone,
            accent: self.accent_color,
            text_size: self.text_size,
            is_dark: self.is_dark(),
        }
    }
}

/// A comparable summary of everything tokens resolve from.
///
/// `PartialEq` rather than `Eq` because `TextSizePreset` compares its scale by
/// float. Equality is only ever used to decide whether cached work must be
/// redone, where float comparison is exactly right.
#[derive(Debug, Clone, Copy, PartialEq, Hash)]
pub struct ThemeSignature {
    pub base_tone: BaseTone,
    pub accent: AccentColor,
    pub text_size: TextSizePreset,
    pub is_dark: bool,
}

/// The semantic token surface. Every view reads colors through here.
///
/// This is the Rust counterpart of Swift's `FXColors`, and carries the same
/// contract: one call site per token, no raw literals in views.
pub struct Colors;

impl Colors {
    // ── Backgrounds ────────────────────────────────────────────────────────

    pub fn bg(t: &Theme) -> Rgba {
        t.palette().bg
    }
    pub fn bg_elevated(t: &Theme) -> Rgba {
        t.palette().bg_elevated
    }
    pub fn bg_surface(t: &Theme) -> Rgba {
        t.palette().bg_surface
    }
    pub fn bg_hover(t: &Theme) -> Rgba {
        t.palette().bg_hover
    }
    pub fn bg_selected(t: &Theme) -> Rgba {
        t.palette().bg_selected
    }
    pub fn bg_pressed(t: &Theme) -> Rgba {
        t.palette().bg_pressed
    }

    // ── Foregrounds ────────────────────────────────────────────────────────
    //
    // Each step clears AA on `bg` and `bg_elevated`; `fg_tertiary` also clears
    // 3:1 on `bg_surface`.

    pub fn fg(t: &Theme) -> Rgba {
        t.palette().fg
    }
    pub fn fg_secondary(t: &Theme) -> Rgba {
        t.palette().fg_secondary
    }
    pub fn fg_tertiary(t: &Theme) -> Rgba {
        t.palette().fg_tertiary
    }
    /// Disabled states and purely decorative glyphs only (~2:1). Never use it
    /// for text or for a control's only icon; use `fg_tertiary` instead.
    pub fn fg_quaternary(t: &Theme) -> Rgba {
        t.palette().fg_quaternary
    }

    // ── Accents ────────────────────────────────────────────────────────────

    pub fn accent(t: &Theme) -> Rgba {
        t.accent_color.color()
    }
    pub fn accent_hover(t: &Theme) -> Rgba {
        t.accent_color.hover_color()
    }
    /// Foreground on a solid accent fill: white, or the tone's darkest ink for
    /// accents too light to carry white text.
    pub fn on_accent(t: &Theme) -> Rgba {
        if t.accent_color.prefers_dark_foreground() {
            t.palette().on_accent_dark
        } else {
            h(0xFFFFFF)
        }
    }
    pub fn accent_secondary(t: &Theme) -> Rgba {
        t.palette().accent_secondary
    }
    pub fn accent_muted(t: &Theme) -> Rgba {
        Rgba {
            a: if t.is_dark() { 0.16 } else { 0.12 },
            ..t.accent_color.color()
        }
    }
    pub fn accent_secondary_muted(t: &Theme) -> Rgba {
        t.palette().accent_secondary_muted
    }

    // ── Semantic ───────────────────────────────────────────────────────────

    pub fn success(t: &Theme) -> Rgba {
        t.palette().success
    }
    pub fn warning(t: &Theme) -> Rgba {
        t.palette().warning
    }
    pub fn error(t: &Theme) -> Rgba {
        t.palette().error
    }
    pub fn info(t: &Theme) -> Rgba {
        t.palette().info
    }

    // ── Semantic tints ─────────────────────────────────────────────────────
    //
    // Banners, badges, and highlighted rows. Never `warning.opacity(…)`.

    pub fn success_muted(t: &Theme) -> Rgba {
        t.palette().success_muted
    }
    pub fn warning_muted(t: &Theme) -> Rgba {
        t.palette().warning_muted
    }
    pub fn error_muted(t: &Theme) -> Rgba {
        t.palette().error_muted
    }
    pub fn info_muted(t: &Theme) -> Rgba {
        t.palette().info_muted
    }

    // ── Diff ───────────────────────────────────────────────────────────────

    pub fn diff_added_bg(t: &Theme) -> Rgba {
        t.palette().diff_added_bg
    }
    pub fn diff_removed_bg(t: &Theme) -> Rgba {
        t.palette().diff_removed_bg
    }
    pub fn diff_added_fg(t: &Theme) -> Rgba {
        t.palette().diff_added_fg
    }
    pub fn diff_removed_fg(t: &Theme) -> Rgba {
        t.palette().diff_removed_fg
    }
    pub fn diff_context_bg(t: &Theme) -> Rgba {
        t.palette().diff_context_bg
    }

    // ── Borders ────────────────────────────────────────────────────────────

    pub fn border(t: &Theme) -> Rgba {
        t.palette().border
    }
    pub fn border_medium(t: &Theme) -> Rgba {
        t.palette().border_medium
    }
    pub fn border_subtle(t: &Theme) -> Rgba {
        t.palette().border_subtle
    }

    // ── Overlays ───────────────────────────────────────────────────────────

    pub fn overlay(t: &Theme) -> Rgba {
        t.palette().overlay
    }
    pub fn overlay_light(t: &Theme) -> Rgba {
        t.palette().overlay_light
    }

    // ── Aliases ────────────────────────────────────────────────────────────
    //
    // Semantic synonyms for the surfaces that appear in several regions.

    pub fn sidebar_bg(t: &Theme) -> Rgba {
        Self::bg_elevated(t)
    }
    pub fn content_bg(t: &Theme) -> Rgba {
        Self::bg_elevated(t)
    }
    pub fn panel_bg(t: &Theme) -> Rgba {
        Self::bg_elevated(t)
    }
    pub fn input_bg(t: &Theme) -> Rgba {
        Self::bg_surface(t)
    }
    pub fn terminal_bg(t: &Theme) -> Rgba {
        t.palette().terminal_bg
    }
}

// ── Contrast measurement ───────────────────────────────────────────────────
//
// Lives beside the palette because the contrast invariants are the contract
// that keeps the palette honest, and the tests below enforce them.

/// WCAG 2 relative luminance of an opaque color.
pub fn relative_luminance(c: Rgba) -> f64 {
    #[inline]
    fn linear(channel: f64) -> f64 {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(f64::from(c.r))
        + 0.7152 * linear(f64::from(c.g))
        + 0.0722 * linear(f64::from(c.b))
}

/// WCAG 2 contrast ratio between two opaque colors.
pub fn contrast_ratio(a: Rgba, b: Rgba) -> f64 {
    let (hi, lo) = (
        relative_luminance(a).max(relative_luminance(b)),
        relative_luminance(a).min(relative_luminance(b)),
    );
    (hi + 0.05) / (lo + 0.05)
}

/// Composite a possibly-translucent `foreground` over `background`.
///
/// This is what the GPUI renderer does at draw time. Call it whenever a
/// *measurement* has to match what is actually painted — for example, the
/// contrast of banner text sitting on a `*_muted` fill.
///
/// The result is always opaque. That is what a contrast measurement needs, and
/// it is what the renderer produces against an opaque window. It makes this the
/// wrong tool for layering translucent tokens onto one another: compositing an
/// absent (fully transparent) layer over a translucent one discards the lower
/// layer and yields opaque black. Check for absence first, or use the layer as
/// it is.
pub fn composite(foreground: Rgba, background: Rgba) -> Rgba {
    if foreground.a >= 1.0 {
        return foreground;
    }
    let a = foreground.a;
    Rgba {
        r: foreground.r * a + background.r * (1.0 - a),
        g: foreground.g * a + background.g * (1.0 - a),
        b: foreground.b * a + background.b * (1.0 - a),
        a: 1.0,
    }
}

/// Contrast of `foreground` composited over `background`.
///
/// Translucent foregrounds are blended over the (opaque) background first, the
/// way they actually render — measuring the raw color would silently ignore
/// every `*_muted` tint.
pub fn contrast(foreground: Rgba, background: Rgba) -> f64 {
    contrast_ratio(composite(foreground, background), background)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_alpha_replaces_only_the_alpha() {
        let c = h(0x6C5CE7).with_alpha(0.3);
        assert_eq!(c.a, 0.3);
        assert_eq!(
            (c.r, c.g, c.b),
            (h(0x6C5CE7).r, h(0x6C5CE7).g, h(0x6C5CE7).b)
        );
    }

    #[test]
    fn compositing_an_opaque_foreground_changes_nothing() {
        let bg = h(0x101010);
        let fg = h(0xFAFAFA);
        assert_eq!(composite(fg, bg), fg);
    }

    #[test]
    fn compositing_half_alpha_midway_between_the_endpoints() {
        let bg = h(0x000000);
        let fg = h(0xFFFFFF);
        let mid = composite(Rgba { a: 0.5, ..fg }, bg);
        assert!((f64::from(mid.r) - 0.5).abs() < 1e-3);
        assert_eq!(mid.a, 1.0);
    }

    #[test]
    fn hex_helper_produces_expected_channels() {
        let c = h(0x6C5CE7);
        assert!((f64::from(c.r) - 108.0 / 255.0).abs() < 1e-6);
        assert!((f64::from(c.g) - 92.0 / 255.0).abs() < 1e-6);
        assert!((f64::from(c.b) - 231.0 / 255.0).abs() < 1e-6);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn mix_endpoints_land_on_the_supplied_stops() {
        let t = ToneScale::ZINC;
        assert_eq!(t.mix(shade::P500, shade::P600, 0.0), t.shade(shade::P500));
        assert_eq!(t.mix(shade::P500, shade::P600, 1.0), t.shade(shade::P600));
    }

    #[test]
    fn tone_scales_have_eleven_shades_ordered_light_to_dark() {
        for tone in BaseTone::ALL {
            let t = ToneScale::for_tone(tone);
            for i in 0..t.hex.len() - 1 {
                assert!(
                    t.hex[i] > t.hex[i + 1],
                    "{:?} shade {} is not lighter than shade {}",
                    tone,
                    i,
                    i + 1
                );
            }
        }
    }

    #[test]
    fn palette_cache_is_stable_across_lookups() {
        for tone in BaseTone::ALL {
            for dark in [true, false] {
                assert_eq!(Palette::get(tone, dark), Palette::get(tone, dark));
            }
        }
    }

    #[test]
    fn cached_palette_matches_a_fresh_generation() {
        for tone in BaseTone::ALL {
            for dark in [true, false] {
                assert_eq!(Palette::get(tone, dark), &Palette::generate(tone, dark));
            }
        }
    }

    #[test]
    fn appearance_resolves_from_mode_then_system() {
        let mut t = Theme::default();
        assert!(t.is_dark(), "dark is the default appearance");

        t.appearance_mode = AppearanceMode::Light;
        assert!(!t.is_dark());

        t.appearance_mode = AppearanceMode::System;
        assert!(t.is_dark(), "system defers to a dark system appearance");
        t.system_appearance = false;
        assert!(!t.is_dark());
    }

    #[test]
    fn display_matches_the_authoritative_label() {
        for m in AppearanceMode::ALL {
            assert_eq!(m.to_string(), m.label());
        }
        for t in BaseTone::ALL {
            assert_eq!(t.to_string(), t.label());
        }
        for a in AccentColor::ALL {
            assert_eq!(a.to_string(), a.label());
        }
        for p in TextSizePreset::ALL {
            assert_eq!(p.to_string(), p.label());
        }
    }

    #[test]
    fn text_presets_increase_and_have_unique_labels() {
        let mut scales = Vec::new();
        let mut labels = Vec::new();
        for p in TextSizePreset::ALL {
            scales.push(p.scale());
            labels.push(p.label());
        }
        assert!(scales.windows(2).all(|w| w[0] < w[1]), "scales {scales:?}");
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "labels {labels:?}");
    }

    #[test]
    fn signature_changes_only_when_tokens_would() {
        let a = Theme::default();
        let mut b = a;
        assert_eq!(a.signature(), b.signature());

        b.accent_color = AccentColor::Rose;
        assert_ne!(a.signature(), b.signature());
    }
}

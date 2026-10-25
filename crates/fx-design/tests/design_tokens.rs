//! The contrast contract.
//!
//! These tests are the reason the palette is trustworthy. They carry over the
//! invariants from the Swift design system, which treats them as binding rather
//! than advisory: if a tone is retuned and one of these fails, the tone is wrong.
//!
//! Run them after touching any foreground, background, or accent token.

use fx_design::components::context_meter::context_meter_palette;
use fx_design::components::provider_status::provider_status_palette;
use fx_design::components::{
    ActivityState, BadgeTone, ChecklistMark, ContextUsage, MenuItem, ProviderIdentity,
    change_indicator_palette, change_indicator_surface, checklist_ink, checklist_surface,
    row_palette,
};
use fx_design::theme::{
    AccentColor, AppearanceMode, BaseTone, Colors, Rgba, TextSizePreset, Theme, composite,
    contrast, h,
};

/// Every tone and appearance combination. A test that must hold everywhere
/// iterates this rather than a hand-written list.
fn all_theme_combinations() -> Vec<(BaseTone, AppearanceMode)> {
    let mut out = Vec::new();
    for tone in BaseTone::ALL {
        for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
            out.push((tone, mode));
        }
    }
    out
}

/// The four status hues paired with their own tinted fills, in the order the
/// palette declares them.
fn status_pairs(t: &Theme) -> [(&'static str, Rgba, Rgba); 4] {
    [
        ("success", Colors::success(t), Colors::success_muted(t)),
        ("warning", Colors::warning(t), Colors::warning_muted(t)),
        ("error", Colors::error(t), Colors::error_muted(t)),
        ("info", Colors::info(t), Colors::info_muted(t)),
    ]
}

/// The surfaces a banner can be painted over.
fn surfaces(t: &Theme) -> [(&'static str, Rgba); 3] {
    [
        ("bg", Colors::bg(t)),
        ("bgElevated", Colors::bg_elevated(t)),
        ("bgSurface", Colors::bg_surface(t)),
    ]
}

fn theme_for(tone: BaseTone, mode: AppearanceMode) -> Theme {
    Theme {
        base_tone: tone,
        appearance_mode: mode,
        system_appearance: true,
        ..Theme::default()
    }
}

#[test]
fn white_on_black_is_the_textbook_ratio() {
    let ratio = contrast(h(0xFFFFFF), h(0x000000));
    assert!((ratio - 21.0).abs() < 0.01, "expected 21:1, got {ratio}:1");
}

/// The sidebar row states a status tile is painted over, composited onto the
/// sidebar the way they render.
fn sidebar_row_surfaces(t: &Theme) -> [(&'static str, Rgba); 3] {
    let sidebar = Colors::sidebar_bg(t);
    [
        ("sidebar", sidebar),
        ("selected", composite(Colors::bg_selected(t), sidebar)),
        ("hovered", composite(Colors::bg_hover(t), sidebar)),
    ]
}

#[test]
fn every_provider_status_glyph_clears_non_text_contrast_on_its_own_tile() {
    // Read from the tile's own palette rather than a copy of its alpha: the
    // copy could only drift. The pulse is swept because its brightest point,
    // the strongest tint, is the hardest case.
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        for provider in [ProviderIdentity::Codex, ProviderIdentity::Claude] {
            for activity in ActivityState::ALL {
                for pulse in [0.0, 0.5, 1.0] {
                    let palette = provider_status_palette(provider, activity, pulse, &t);
                    for (surface_name, surface) in sidebar_row_surfaces(&t) {
                        let ratio = contrast(palette.glyph, composite(palette.fill, surface));
                        assert!(
                            ratio >= 3.0,
                            "{tone:?} {mode:?} {provider:?} {activity:?} at pulse {pulse}: glyph on its tint over {surface_name} is {ratio}:1"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_error_glyph_clears_non_text_contrast_on_the_provider_tint() {
    // A failed tile keeps its provider's tint and swaps only the glyph, so
    // `error` has to hold up on a hue that is not its own.
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        for provider in [ProviderIdentity::Codex, ProviderIdentity::Claude] {
            let palette = provider_status_palette(provider, ActivityState::Error, 1.0, &t);
            assert_eq!(palette.glyph, Colors::error(&t));
            assert_eq!(palette.fill.a, 0.12, "{tone:?} {mode:?} {provider:?}");
            for (surface_name, surface) in sidebar_row_surfaces(&t) {
                let ratio = contrast(palette.glyph, composite(palette.fill, surface));
                assert!(
                    ratio >= 3.0,
                    "{tone:?} {mode:?}: error on the {provider:?} tint over {surface_name} is {ratio}:1"
                );
            }
        }
    }
}

#[test]
fn the_waiting_pulse_stays_inside_its_documented_range() {
    let t = theme_for(BaseTone::Zinc, AppearanceMode::Dark);
    let fill = |pulse| {
        provider_status_palette(ProviderIdentity::Codex, ActivityState::Waiting, pulse, &t)
            .fill
            .a
    };
    assert!((fill(0.0) - 0.08).abs() < 1e-6);
    assert!((fill(1.0) - 0.18).abs() < 1e-6);
    assert_eq!(fill(7.0), fill(1.0), "an out-of-range pulse is clamped");
}

#[test]
fn the_context_meter_arc_clears_non_text_contrast_on_the_composer_at_every_level() {
    // Light-mode `warning` was 2.90:1 here; the arc alone carries the level, so
    // it needs the 3:1 a meaningful graphic does.
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        for provider in [ProviderIdentity::Codex, ProviderIdentity::Claude] {
            for used in [
                None,
                Some(0),
                Some(50),
                Some(75),
                Some(89),
                Some(90),
                Some(100),
            ] {
                let usage = ContextUsage {
                    used,
                    limit: Some(100),
                };
                let ring = context_meter_palette(usage, provider, &t).ring;
                let ratio = contrast(ring, Colors::composer_bg(&t));
                assert!(
                    ratio >= 3.0,
                    "{tone:?} {mode:?} {provider:?} at {used:?}%: the arc is {ratio}:1 on the composer"
                );
            }
        }
    }
}

#[test]
fn warning_ink_clears_non_text_contrast_on_elevated_surfaces_and_stays_amber() {
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        for (name, surface) in surfaces(&t) {
            let ratio = contrast(Colors::warning_ink(&t), surface);
            assert!(
                ratio >= 3.0,
                "{tone:?} {mode:?}: warningInk on {name} is {ratio}:1"
            );
        }
        let ink = Colors::warning_ink(&t);
        assert!(
            ink.r > ink.g && ink.g > ink.b,
            "{tone:?} {mode:?}: warningInk is no longer an amber"
        );
        if mode == AppearanceMode::Dark {
            assert_eq!(ink, Colors::warning(&t), "dark mode needs no step down");
        }
    }
}

#[test]
fn every_checklist_row_is_legible_on_its_surface_with_every_accent() {
    // Text clears AA, a done row included: receding is not the same as
    // illegible. Each mark is the only thing that says where its step is, so it
    // clears the 3:1 non-text floor.
    for (tone, mode) in all_theme_combinations() {
        for accent in AccentColor::ALL {
            let t = Theme {
                accent_color: accent,
                ..theme_for(tone, mode)
            };
            let surface = checklist_surface(&t);
            for mark in ChecklistMark::ALL {
                let ink = checklist_ink(mark, &t);
                let text = contrast(ink.text, surface);
                let glyph = contrast(ink.mark, surface);
                assert!(
                    text >= 4.5,
                    "{tone:?} {mode:?} {accent:?}: {mark:?} text is {text}:1"
                );
                assert!(
                    glyph >= 3.0,
                    "{tone:?} {mode:?} {accent:?}: {mark:?} mark is {glyph}:1"
                );
            }
        }
    }
}

#[test]
fn every_badge_label_clears_aa_on_its_fill_over_every_surface_it_sits_on() {
    // A badge's fill is translucent, and badges sit on cards (`bgSurface`) as
    // well as on panels. Measured over the panel alone, the dark "Ready" label
    // on a provider card was 4.2:1.
    for (tone, mode) in all_theme_combinations() {
        for accent in AccentColor::ALL {
            let t = Theme {
                accent_color: accent,
                ..theme_for(tone, mode)
            };
            for (surface_name, surface) in surfaces(&t) {
                for badge in BadgeTone::ALL {
                    let fill = composite(badge.fill(&t), surface);
                    let ratio = contrast(badge.text_ink(&t), fill);
                    assert!(
                        ratio >= 4.5,
                        "{tone:?} {mode:?} {accent:?}: the {badge:?} label over {surface_name} is {ratio}:1"
                    );
                }
            }
        }
    }
}

#[test]
fn the_change_counts_clear_aa_on_the_title_bar_selected_or_not() {
    // 12pt text. Light-mode `success` was 3.4:1 here, and 2.9:1 with the
    // inspector open; a selected indicator keeps its fill under the pointer.
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        let bar = change_indicator_surface(&t);
        for selected in [false, true] {
            let ink = change_indicator_palette(selected, false, &t);
            for hovered in [false, true] {
                let fill = composite(
                    change_indicator_palette(selected, hovered, &t).background,
                    bar,
                );
                for (name, color) in [
                    ("added", ink.added),
                    ("removed", ink.removed),
                    ("files", ink.files),
                ] {
                    let ratio = contrast(color, fill);
                    assert!(
                        ratio >= 4.5,
                        "{tone:?} {mode:?}: the {name} count is {ratio}:1 (selected {selected}, hovered {hovered})"
                    );
                }
            }
        }
    }
}

#[test]
fn a_menu_checkmark_clears_non_text_contrast_with_every_accent() {
    // The checkmark alone says which choice is current. The raw accent was
    // 2.15:1 for dark violet on a menu, 1.9:1 hovered.
    for (tone, mode) in all_theme_combinations() {
        for accent in AccentColor::ALL {
            let t = Theme {
                accent_color: accent,
                ..theme_for(tone, mode)
            };
            let surface = Colors::bg_surface(&t);
            let item = MenuItem::new("current", "Current").selected(true);
            for hovered in [false, true] {
                let palette = row_palette(&item, hovered, &t);
                let fill = composite(palette.background, surface);
                let ratio = contrast(palette.check, fill);
                assert!(
                    ratio >= 3.0,
                    "{tone:?} {mode:?} {accent:?}: the checkmark is {ratio}:1 (hovered {hovered})"
                );
            }
        }
    }
}

#[test]
fn text_tokens_meet_their_contrast_floors_in_every_tone_and_mode() {
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        let where_ = format!("{tone:?} {mode:?}");

        // Captions, section headers, and menu subtitles sit on the window and
        // panel surfaces, so every text step needs AA on both.
        for (surface, background) in [
            ("bg", Colors::bg(&t)),
            ("bgElevated", Colors::bg_elevated(&t)),
        ] {
            let floors: [(&str, fx_design::theme::Rgba, f64); 4] = [
                ("fg", Colors::fg(&t), 7.0),
                ("fgSecondary", Colors::fg_secondary(&t), 4.5),
                ("fgTertiary", Colors::fg_tertiary(&t), 4.5),
                ("accentSecondary", Colors::accent_secondary(&t), 4.5),
            ];
            for (token, color, floor) in floors {
                let ratio = contrast(color, background);
                assert!(
                    ratio >= floor,
                    "{where_}: {token} on {surface} is {ratio}:1, needs {floor}:1"
                );
            }
        }

        // Menus, cards, and inputs use bgSurface. Tertiary text there is
        // secondary information, so it only has to clear 3:1.
        let surface = Colors::bg_surface(&t);
        let secondary = contrast(Colors::fg_secondary(&t), surface);
        assert!(
            secondary >= 4.5,
            "{where_}: fgSecondary on bgSurface is {secondary}:1"
        );
        let tertiary = contrast(Colors::fg_tertiary(&t), surface);
        assert!(
            tertiary >= 3.0,
            "{where_}: fgTertiary on bgSurface is {tertiary}:1"
        );

        // The ladder must descend: each step weaker than the one above it.
        let ladder = [
            contrast(Colors::fg(&t), Colors::bg_elevated(&t)),
            contrast(Colors::fg_secondary(&t), Colors::bg_elevated(&t)),
            contrast(Colors::fg_tertiary(&t), Colors::bg_elevated(&t)),
            contrast(Colors::fg_quaternary(&t), Colors::bg_elevated(&t)),
        ];
        assert!(
            ladder.windows(2).all(|w| w[0] > w[1]),
            "{where_}: foreground steps are out of order {ladder:?}"
        );
    }
}

#[test]
fn labels_on_solid_accent_fills_stay_legible_for_every_accent() {
    for accent in AccentColor::ALL {
        for tone in BaseTone::ALL {
            for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
                let t = Theme {
                    accent_color: accent,
                    ..theme_for(tone, mode)
                };
                let ratio = contrast(Colors::on_accent(&t), Colors::accent(&t));
                assert!(
                    ratio >= 3.0,
                    "{accent:?} {tone:?} {mode:?}: onAccent on accent is {ratio}:1"
                );
            }
        }

        // Dark ink is reserved for accents that genuinely cannot carry white.
        let white = contrast(h(0xFFFFFF), accent.color());
        assert_eq!(
            accent.prefers_dark_foreground(),
            white < 3.0,
            "{accent:?}: white on accent is {white}:1"
        );
    }
}

#[test]
fn status_tints_adapt_to_the_appearance_mode() {
    let light = theme_for(BaseTone::Zinc, AppearanceMode::Light);
    let dark = theme_for(BaseTone::Zinc, AppearanceMode::Dark);

    let light_tints = [
        Colors::success_muted(&light),
        Colors::warning_muted(&light),
        Colors::error_muted(&light),
        Colors::info_muted(&light),
        Colors::accent_secondary_muted(&light),
    ];
    let dark_tints = [
        Colors::success_muted(&dark),
        Colors::warning_muted(&dark),
        Colors::error_muted(&dark),
        Colors::info_muted(&dark),
        Colors::accent_secondary_muted(&dark),
    ];

    for (l, d) in light_tints.iter().zip(&dark_tints) {
        assert_ne!(l, d, "a status tint did not change between modes");
    }

    // Each tint must remain distinguishable from the others within a mode.
    let mut dark_sorted: Vec<u32> = dark_tints.iter().map(|c| u32::from(*c)).collect();
    dark_sorted.sort_unstable();
    dark_sorted.dedup();
    assert_eq!(
        dark_sorted.len(),
        dark_tints.len(),
        "two dark status tints resolved to the same color"
    );
}

#[test]
fn banner_text_clears_aa_on_every_muted_fill() {
    // This is the invariant that actually governs readability. A `*Muted` token
    // is a *banner fill*, painted translucently over the window or panel behind
    // it, and the words on it are `fg` / `fgSecondary`. The same-hue color is
    // used for the banner's icon, not its text.
    //
    // The raw tint has no effective color until the renderer composites it, so
    // composite first and measure the result.
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        for (surface_name, surface) in surfaces(&t) {
            let fills: [(&str, Rgba); 5] = [
                ("successMuted", Colors::success_muted(&t)),
                ("warningMuted", Colors::warning_muted(&t)),
                ("errorMuted", Colors::error_muted(&t)),
                ("infoMuted", Colors::info_muted(&t)),
                ("accentSecondaryMuted", Colors::accent_secondary_muted(&t)),
            ];
            for (fill_name, tint) in fills {
                let fill = composite(tint, surface);
                for (ink_name, ink) in [
                    ("fg", Colors::fg(&t)),
                    ("fgSecondary", Colors::fg_secondary(&t)),
                ] {
                    let ratio = contrast(ink, fill);
                    assert!(
                        ratio >= 4.5,
                        "{tone:?} {mode:?}: {ink_name} on {fill_name} over {surface_name} is {ratio}:1"
                    );
                }
            }
        }
    }
}

#[test]
fn selected_text_stays_legible_and_visible_on_every_transcript_surface() {
    // Selection is painted under the glyphs, so the words on it must still
    // clear AA. It is also painted over the reader's own prompt bubble, which is
    // already an accent wash, so the highlight has to stay distinguishable there.
    for (tone, mode) in all_theme_combinations() {
        for accent in AccentColor::ALL {
            let t = Theme {
                accent_color: accent,
                ..theme_for(tone, mode)
            };
            let bubble = composite(Colors::accent_muted(&t), Colors::bg(&t));
            for (surface_name, surface) in [
                ("bg", Colors::bg(&t)),
                ("bgElevated", Colors::bg_elevated(&t)),
                ("bgSurface", Colors::bg_surface(&t)),
                ("userBubble", bubble),
            ] {
                let selected = composite(Colors::text_selection(&t), surface);
                let ratio = contrast(Colors::fg(&t), selected);
                assert!(
                    ratio >= 4.5,
                    "{tone:?} {mode:?} {accent:?}: fg on a selection over {surface_name} is {ratio}:1"
                );
                assert!(
                    contrast(selected, surface) > 1.1,
                    "{tone:?} {mode:?} {accent:?}: a selection over {surface_name} is invisible"
                );
            }
        }
    }
}

#[test]
fn a_muted_tint_is_actually_translucent() {
    // Guards the premise of the tests above: if these ever became opaque, the
    // contrast checks would silently stop testing anything.
    for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
        let t = theme_for(BaseTone::Zinc, mode);
        for (name, tint) in [
            ("successMuted", Colors::success_muted(&t)),
            ("warningMuted", Colors::warning_muted(&t)),
            ("errorMuted", Colors::error_muted(&t)),
            ("infoMuted", Colors::info_muted(&t)),
        ] {
            assert!(
                tint.a < 1.0,
                "{mode:?}: {name} is opaque, so it would cover the surface instead of tinting it"
            );
            assert!(tint.a > 0.0, "{mode:?}: {name} is fully transparent");
        }
    }
}

#[test]
fn status_ink_meets_its_documented_floor_on_its_own_tint() {
    // Where the same-hue color *is* used as an icon on its own tint, WCAG asks
    // for 3:1. The measured floor across all tones and both modes is 2.29:1,
    // reached by light-mode `warning` (amber-600 under a 10% amber tint).
    // Light-mode `success` likewise lands at 2.67:1. Dark mode clears 3:1
    // everywhere.
    //
    // This test pins the floor so a future retune cannot make it worse, and
    // records the shortfall rather than hiding it. Raising it to a true 3:1
    // would mean darkening the light-mode `warning` and `success` values, which
    // is a palette change, not a port.
    const DOCUMENTED_FLOOR: f64 = 2.25;
    const TARGET: f64 = 3.0;

    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        for (surface_name, surface) in surfaces(&t) {
            for (name, ink, tint) in status_pairs(&t) {
                let ratio = contrast(ink, composite(tint, surface));
                assert!(
                    ratio >= DOCUMENTED_FLOOR,
                    "{tone:?} {mode:?}: {name} ink on its own tint over {surface_name} fell to {ratio}:1, below the documented {DOCUMENTED_FLOOR}:1 floor"
                );
                if ratio < TARGET {
                    // Dark mode has no known shortfall; if light mode regresses
                    // further this branch will start firing for tones it did not
                    // before, which is the signal we want.
                    assert!(
                        mode == AppearanceMode::Light,
                        "{tone:?}: {name} on its tint over {surface_name} is {ratio}:1 in dark mode, where every token is expected to clear {TARGET}:1"
                    );
                }
            }
        }
    }
}

#[test]
fn dark_mode_status_ink_clears_the_non_text_target_everywhere() {
    for tone in BaseTone::ALL {
        let t = theme_for(tone, AppearanceMode::Dark);
        for (surface_name, surface) in surfaces(&t) {
            for (name, ink, tint) in status_pairs(&t) {
                let ratio = contrast(ink, composite(tint, surface));
                assert!(
                    ratio >= 3.0,
                    "{tone:?}: {name} on its tint over {surface_name} is {ratio}:1 in dark mode"
                );
            }
        }
    }
}

#[test]
fn diff_foreground_is_legible_on_its_own_diff_background() {
    for (tone, mode) in all_theme_combinations() {
        let t = theme_for(tone, mode);
        let added = contrast(Colors::diff_added_fg(&t), Colors::diff_added_bg(&t));
        assert!(
            added >= 3.0,
            "{tone:?} {mode:?}: diffAddedFg on diffAddedBg is {added}:1"
        );
        let removed = contrast(Colors::diff_removed_fg(&t), Colors::diff_removed_bg(&t));
        assert!(
            removed >= 3.0,
            "{tone:?} {mode:?}: diffRemovedFg on diffRemovedBg is {removed}:1"
        );
    }
}

#[test]
fn a_theme_change_reaches_every_token_it_should() {
    let dark = theme_for(BaseTone::Zinc, AppearanceMode::Dark);
    let light = Theme {
        appearance_mode: AppearanceMode::Light,
        ..dark
    };

    for (name, a, b) in [
        ("bg", Colors::bg(&dark), Colors::bg(&light)),
        (
            "diffAddedBg",
            Colors::diff_added_bg(&dark),
            Colors::diff_added_bg(&light),
        ),
        (
            "successMuted",
            Colors::success_muted(&dark),
            Colors::success_muted(&light),
        ),
    ] {
        assert_ne!(a, b, "{name} ignored the appearance change");
    }

    // Switching back must reproduce the original exactly, because palettes are
    // cached rather than regenerated per read.
    let back = theme_for(BaseTone::Zinc, AppearanceMode::Dark);
    assert_eq!(Colors::bg(&dark), Colors::bg(&back));
    assert_eq!(Colors::diff_added_bg(&dark), Colors::diff_added_bg(&back));

    // A tone change must move the background too.
    let stone = Theme {
        base_tone: BaseTone::Stone,
        ..back
    };
    assert_ne!(Colors::bg(&back), Colors::bg(&stone));
}

#[test]
fn accent_muted_tracks_the_accent_and_the_mode() {
    let violet = Theme {
        accent_color: AccentColor::Violet,
        ..theme_for(BaseTone::Zinc, AppearanceMode::Dark)
    };
    let rose = Theme {
        accent_color: AccentColor::Rose,
        ..violet
    };
    assert_ne!(Colors::accent_muted(&violet), Colors::accent_muted(&rose));

    let dark_muted = Colors::accent_muted(&violet);
    let light_muted = Colors::accent_muted(&Theme {
        appearance_mode: AppearanceMode::Light,
        ..violet
    });
    assert!(
        light_muted.a < dark_muted.a,
        "light mode should use a lighter accent tint"
    );
}

#[test]
fn the_system_appearance_only_matters_when_the_mode_is_system() {
    let mut pinned_dark = theme_for(BaseTone::Zinc, AppearanceMode::Dark);
    pinned_dark.system_appearance = false;
    assert!(pinned_dark.is_dark(), "an explicit dark mode must win");

    let mut pinned_light = theme_for(BaseTone::Zinc, AppearanceMode::Light);
    pinned_light.system_appearance = true;
    assert!(!pinned_light.is_dark(), "an explicit light mode must win");

    let following = Theme {
        appearance_mode: AppearanceMode::System,
        system_appearance: true,
        ..pinned_dark
    };
    assert!(following.is_dark());
}

#[test]
fn the_text_size_preset_rescales_type_without_disturbing_color() {
    let standard = theme_for(BaseTone::Zinc, AppearanceMode::Dark);
    let large = Theme {
        text_size: TextSizePreset::Large,
        ..standard
    };
    assert_eq!(
        Colors::fg(&standard),
        Colors::fg(&large),
        "text size must not alter color tokens"
    );
    assert!(
        large.text_scale() > standard.text_scale(),
        "large preset should scale type up"
    );
}

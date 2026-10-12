//! Prints measured contrast for the palette so a retune can be judged against
//! numbers rather than impressions. Run with:
//!
//! ```sh
//! cargo run -p fx-design --example contrast_report
//! ```

use fx_design::theme::{AccentColor, AppearanceMode, BaseTone, Colors, Theme, composite, contrast};

fn main() {
    println!("status ink on its own tint, composited over each surface\n");
    for mode in [AppearanceMode::Dark, AppearanceMode::Light] {
        println!("--- {mode:?} ---");
        println!(
            "{:<8} {:<8} {:>8} {:>8} {:>8}",
            "tone", "token", "bg", "bgElev", "bgSurf"
        );
        for tone in BaseTone::ALL {
            let t = Theme {
                base_tone: tone,
                appearance_mode: mode,
                ..Default::default()
            };
            let surfaces = [
                Colors::bg(&t),
                Colors::bg_elevated(&t),
                Colors::bg_surface(&t),
            ];
            let pairs = [
                ("success", Colors::success(&t), Colors::success_muted(&t)),
                ("warning", Colors::warning(&t), Colors::warning_muted(&t)),
                ("error", Colors::error(&t), Colors::error_muted(&t)),
                ("info", Colors::info(&t), Colors::info_muted(&t)),
            ];
            for (name, ink, tint) in pairs {
                let ratios: Vec<String> = surfaces
                    .iter()
                    .map(|s| format!("{:>6.2}", contrast(ink, composite(tint, *s))))
                    .collect();
                println!("{tone:<8} {name:<8} {}", ratios.join(" "));
            }
        }
        println!();
    }

    println!("--- onAccent on accent (target 3.0) ---");
    for accent in AccentColor::ALL {
        let mut worst = f64::INFINITY;
        for tone in BaseTone::ALL {
            for mode in [AppearanceMode::Dark, AppearanceMode::Light] {
                let t = Theme {
                    accent_color: accent,
                    base_tone: tone,
                    appearance_mode: mode,
                    ..Default::default()
                };
                worst = worst.min(contrast(Colors::on_accent(&t), Colors::accent(&t)));
            }
        }
        println!("{accent:<9} worst {worst:>6.2}:1");
    }

    println!("\n--- text floors (fg>=7, fgSecondary/fgTertiary>=4.5 on bg & bgElevated) ---");
    for mode in [AppearanceMode::Dark, AppearanceMode::Light] {
        let t = Theme {
            appearance_mode: mode,
            ..Default::default()
        };
        println!(
            "{mode:?}: fg {:>5.2}  fgSecondary {:>5.2}  fgTertiary {:>5.2}",
            contrast(Colors::fg(&t), Colors::bg(&t)),
            contrast(Colors::fg_secondary(&t), Colors::bg(&t)),
            contrast(Colors::fg_tertiary(&t), Colors::bg(&t)),
        );
    }
}

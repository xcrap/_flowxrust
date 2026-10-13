//! Reversible settings for the independent models that name new chats.

use fx_agent::Model;
use fx_design::components::segmented::{SegmentedOption, segmented_control};
use fx_design::components::{
    MenuItem, MenuLayer, MenuModel, MenuSection, MenuTrigger, card, menu_trigger, section_header,
};
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, Spacing, global};
use fx_store::AutoTitlePreferences;
use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Entity, Render, SharedString, Window, div};

use crate::provider_catalog;

const PREFIX: &str = "settings-titles:";

pub(crate) struct TitleSettings {
    draft: AutoTitlePreferences,
    baseline: AutoTitlePreferences,
    menu_layer: Entity<MenuLayer>,
}

impl TitleSettings {
    pub(crate) fn new(
        draft: AutoTitlePreferences,
        menu_layer: Entity<MenuLayer>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            baseline: draft.clone(),
            draft,
            menu_layer,
        })
    }

    pub(crate) fn values(&self) -> AutoTitlePreferences {
        self.draft.clone()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.draft != self.baseline
    }

    pub(crate) fn reset(&mut self, cx: &mut Context<Self>) {
        self.draft = self.baseline.clone();
        self.dismiss_menu(cx);
        cx.notify();
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.draft.enabled = enabled;
        cx.notify();
    }

    pub(crate) fn menu_is_open(&self, cx: &App) -> bool {
        self.menu_layer
            .read(cx)
            .owner()
            .is_some_and(|owner| owner.starts_with(PREFIX))
    }

    pub(crate) fn dismiss_menu(&self, cx: &mut App) {
        if self.menu_is_open(cx) {
            self.menu_layer.update(cx, |menu, cx| {
                menu.dismiss();
                cx.notify();
            });
        }
    }

    pub(crate) fn apply(&mut self, owner: &str, chosen: &str, cx: &mut Context<Self>) -> bool {
        let Some(provider) = owner.strip_prefix(PREFIX) else {
            return false;
        };
        let current = match provider {
            "codex" => &mut self.draft.codex_model_id,
            "claude" => &mut self.draft.claude_model_id,
            _ => return false,
        };
        let models = provider_catalog::models(provider);
        let default = default_model_id(provider);
        if chosen != current.as_str()
            && chosen != default
            && !models.iter().any(|model| model.id == chosen)
        {
            return false;
        }
        *current = chosen.to_owned();
        cx.notify();
        true
    }

    fn model_row(&self, provider: &str, label: &'static str, cx: &App) -> AnyElement {
        let theme = global::current();
        let selected = if provider == "codex" {
            &self.draft.codex_model_id
        } else {
            &self.draft.claude_model_id
        };
        let models = provider_catalog::models(provider);
        let display = provider_catalog::model(provider, selected)
            .map(|model| model.display_name)
            .unwrap_or_else(|| {
                if provider == "claude" && selected == "haiku" {
                    "Haiku".into()
                } else {
                    selected.clone()
                }
            });
        let owner = format!("{PREFIX}{provider}");
        let selector = owner.clone();
        let expanded = self.menu_layer.read(cx).is_owner(&owner);
        card(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .justify_between()
                .gap(px_len(Spacing::MD))
                .child(
                    div()
                        .flex_shrink_0()
                        .fx_body()
                        .text_color(Colors::fg(&theme))
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .debug_selector(move || selector.clone())
                        .child(menu_trigger(
                            MenuTrigger::new(
                                model_menu(provider, selected, &models),
                                SharedString::from(owner),
                                SharedString::from(display),
                            )
                            .expanded(expanded),
                            &self.menu_layer,
                        )),
                ),
            Spacing::MD,
        )
        .into_any_element()
    }
}

impl Render for TitleSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let this = cx.entity();
        div().w_full().min_w_0().flex().flex_col().gap(px_len(Spacing::MD))
            .child(section_header("Chat names"))
            .child(div().fx_caption().text_color(Colors::fg_secondary(&theme)).child("Generate a short name after the first successful exchange in a new chat."))
            .child(
                div().w_full().flex().items_center().justify_between().gap(px_len(Spacing::MD))
                    .child(div().fx_body().child("Automatic names"))
                    .child(div().debug_selector(|| "settings-titles-enabled".into()).child(segmented_control(
                        &[SegmentedOption::new(false, "Off"), SegmentedOption::new(true, "On")],
                        &self.draft.enabled,
                        move |enabled, _, cx| this.update(cx, |editor, cx| editor.set_enabled(*enabled, cx)),
                    ))),
            )
            .child(div().fx_caption().text_color(Colors::fg_secondary(&theme)).child("Each chat uses its own provider and the title model below. These choices are independent of your conversation models."))
            .child(self.model_row("codex", "Codex title model", cx))
            .child(self.model_row("claude", "Claude title model", cx))
            .child(div().fx_caption().text_color(Colors::fg_tertiary(&theme)).child("One separate model request per new chat. Names you set yourself are kept."))
    }
}

fn default_model_id(provider: &str) -> String {
    let defaults = AutoTitlePreferences::default();
    if provider == "codex" {
        defaults.codex_model_id
    } else {
        defaults.claude_model_id
    }
}

fn model_menu(provider: &str, selected: &str, models: &[Model]) -> MenuModel {
    let mut items: Vec<_> = models
        .iter()
        .map(|model| MenuItem::new(&model.id, &model.display_name).selected(model.id == selected))
        .collect();
    let default = default_model_id(provider);
    if !models.iter().any(|model| model.id == default) {
        items.insert(
            0,
            MenuItem::new(
                &default,
                if provider == "claude" {
                    "Haiku"
                } else {
                    default.as_str()
                },
            )
            .subtitle("Default title model")
            .selected(selected == default),
        );
    }
    if selected != default && !models.iter().any(|model| model.id == selected) {
        items.push(
            MenuItem::new(selected, selected)
                .subtitle("Saved model; not in the current provider list")
                .selected(true),
        );
    }
    MenuModel::new(vec![MenuSection::new("title-models", items)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_unavailable_title_model_and_the_lightweight_default_remain_choices() {
        let menu = model_menu("claude", "private-title-model", &[]);
        let items = &menu.sections()[0].items;
        assert!(
            items
                .iter()
                .any(|item| item.id == "haiku" && !item.selected)
        );
        assert!(
            items
                .iter()
                .any(|item| item.id == "private-title-model" && item.selected)
        );
        assert_eq!(items.len(), 2);
    }
}

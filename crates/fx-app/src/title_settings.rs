//! Reversible settings for the shared provider that names all new chats.

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
        match owner.strip_prefix(PREFIX) {
            Some("provider") if matches!(chosen, "codex" | "claude") => {
                if self.draft.provider_id != chosen {
                    self.draft.provider_id = chosen.into();
                    self.draft.model_id = AutoTitlePreferences::default_model_id(chosen).into();
                }
            }
            Some("model") => {
                let models = provider_catalog::models(&self.draft.provider_id);
                let default = AutoTitlePreferences::default_model_id(&self.draft.provider_id);
                if chosen != self.draft.model_id
                    && chosen != default
                    && !models.iter().any(|model| model.id == chosen)
                {
                    return false;
                }
                self.draft.model_id = chosen.into();
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    fn picker_row(
        &self,
        field: &str,
        label: &'static str,
        display: String,
        model: MenuModel,
        cx: &App,
    ) -> AnyElement {
        let theme = global::current();
        let owner = format!("{PREFIX}{field}");
        let selector = owner.clone();
        let expanded = self.menu_layer.read(cx).is_owner(&owner);
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
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&theme))
                    .child(label),
            )
            .child(
                div()
                    .min_w_0()
                    .debug_selector(move || selector.clone())
                    .child(menu_trigger(
                        MenuTrigger::new(
                            model,
                            SharedString::from(owner),
                            SharedString::from(display),
                        )
                        .expanded(expanded),
                        &self.menu_layer,
                    )),
            )
            .into_any_element()
    }
}

impl Render for TitleSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let this = cx.entity();
        let provider = &self.draft.provider_id;
        let models = provider_catalog::models(provider);
        let display = provider_catalog::model(provider, &self.draft.model_id)
            .map(|model| model.display_name)
            .unwrap_or_else(|| {
                if provider == "claude" && self.draft.model_id == "haiku" {
                    "Haiku".into()
                } else {
                    self.draft.model_id.clone()
                }
            });
        div().w_full().min_w_0().flex().flex_col().gap(px_len(Spacing::MD))
            .debug_selector(|| "settings-chat-names".into())
            .child(section_header("Chat names"))
            .child(div().fx_caption().text_color(Colors::fg_secondary(&theme)).child("Name every new chat from its first prompt using the same provider and model."))
            .child(card(
                div().w_full().min_w_0().flex().flex_col().gap(px_len(Spacing::SM))
                    .child(
                        div().w_full().flex().items_center().justify_between().gap(px_len(Spacing::MD))
                            .child(div().fx_body().child("Automatic names"))
                            .child(div().debug_selector(|| "settings-titles-enabled".into()).child(segmented_control(
                                &[SegmentedOption::new(false, "Off"), SegmentedOption::new(true, "On")],
                                &self.draft.enabled,
                                move |enabled, _, cx| this.update(cx, |editor, cx| editor.set_enabled(*enabled, cx)),
                            ))),
                    )
                    .child(self.picker_row("provider", "Provider", if provider == "claude" { "Claude" } else { "Codex" }.into(), provider_menu(provider), cx))
                    .child(self.picker_row("model", "Model", display, model_menu(provider, &self.draft.model_id, &models), cx)),
                Spacing::MD,
            ))
            .child(div().fx_caption().text_color(Colors::fg_tertiary(&theme)).child("One naming request per new chat. Names you set yourself are kept."))
    }
}

fn provider_menu(selected: &str) -> MenuModel {
    MenuModel::new(vec![MenuSection::new(
        "title-provider",
        vec![
            MenuItem::new("codex", "Codex").selected(selected == "codex"),
            MenuItem::new("claude", "Claude").selected(selected == "claude"),
        ],
    )])
}

fn model_menu(provider: &str, selected: &str, models: &[Model]) -> MenuModel {
    let mut items: Vec<_> = models
        .iter()
        .map(|model| MenuItem::new(&model.id, &model.display_name).selected(model.id == selected))
        .collect();
    let default = AutoTitlePreferences::default_model_id(provider);
    if !models.iter().any(|model| model.id == default) {
        items.insert(
            0,
            MenuItem::new(
                default,
                if provider == "claude" {
                    "Haiku"
                } else {
                    default
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
    fn changing_the_shared_provider_selects_its_lightweight_model_and_reset_restores_both() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let menus = cx.new(|_| MenuLayer::default());
            let baseline = AutoTitlePreferences {
                model_id: "saved-small-model".into(),
                ..AutoTitlePreferences::default()
            };
            let editor = TitleSettings::new(baseline.clone(), menus, cx);
            editor.update(cx, |editor, cx| {
                assert!(editor.apply("settings-titles:provider", "codex", cx));
                assert_eq!(editor.values(), baseline);
                assert!(editor.apply("settings-titles:provider", "claude", cx));
                assert_eq!(editor.values().provider_id, "claude");
                assert_eq!(editor.values().model_id, "haiku");
                assert!(!editor.apply("settings-titles:provider", "unknown", cx));
                assert!(!editor.apply("settings-titles:model", "foreign-model", cx));
                assert_eq!(editor.values().model_id, "haiku");
                assert!(editor.apply("settings-titles:provider", "codex", cx));
                assert_eq!(editor.values().model_id, "gpt-5.6-luna");
                editor.reset(cx);
                assert_eq!(editor.values(), baseline);
                assert!(!editor.is_dirty());
            });
        });
    }

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

//! Reversible defaults for new Codex and Claude chats.

use fx_agent::Model;
use fx_core::AgentAccess;
use fx_design::components::{
    MenuItem, MenuLayer, MenuModel, MenuSection, MenuTrigger, card, menu_trigger, section_header,
};
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, Spacing, global};
use fx_store::ProviderDefaults;
use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Entity, Render, SharedString, Window, div};

use crate::provider_catalog;

const PREFIX: &str = "settings-defaults:";
const INHERIT: &str = "__inherit";
const REFRESH: &str = "__refresh";

pub(crate) struct ProviderSettings {
    codex: ProviderDefaults,
    claude: ProviderDefaults,
    baseline: (ProviderDefaults, ProviderDefaults),
    menu_layer: Entity<MenuLayer>,
}

impl ProviderSettings {
    pub(crate) fn new(
        codex: ProviderDefaults,
        claude: ProviderDefaults,
        menu_layer: Entity<MenuLayer>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            baseline: (codex.clone(), claude.clone()),
            codex,
            claude,
            menu_layer,
        })
    }

    pub(crate) fn values(&self) -> (ProviderDefaults, ProviderDefaults) {
        (self.codex.clone(), self.claude.clone())
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.values() != self.baseline
    }

    pub(crate) fn reset(&mut self, cx: &mut Context<Self>) {
        (self.codex, self.claude) = self.baseline.clone();
        self.dismiss_menu(cx);
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
            self.menu_layer.update(cx, |layer, cx| {
                layer.dismiss();
                cx.notify();
            });
        }
    }

    /// Route only this editor's choices; unrelated app menus remain untouched.
    pub(crate) fn apply(&mut self, owner: &str, chosen: &str, cx: &mut Context<Self>) -> bool {
        let Some((provider, field)) = owner.strip_prefix(PREFIX).and_then(|s| s.split_once(':'))
        else {
            return false;
        };
        let value = match provider {
            "codex" => &mut self.codex,
            "claude" => &mut self.claude,
            _ => return false,
        };
        if field == "model" && chosen == REFRESH {
            provider_catalog::refresh_now(provider);
        } else if !apply_choice(value, provider, field, chosen) {
            return false;
        }
        cx.notify();
        true
    }

    fn provider_section(
        &self,
        provider: &'static str,
        title: &'static str,
        cx: &App,
    ) -> AnyElement {
        let theme = global::current();
        let defaults = if provider == "codex" {
            &self.codex
        } else {
            &self.claude
        };
        let models = provider_catalog::models(provider);
        let selected = selected_model(provider, defaults);
        let capabilities = selected_capabilities(provider, defaults);
        let mut content = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::SM))
            .child(div().fx_body().child(title))
            .child(self.picker_row(
                provider,
                "model",
                "Model",
                model_label(provider, defaults, selected.as_ref()),
                model_menu(provider, defaults, &models),
                cx,
            ))
            .child(
                self.picker_row(
                    provider,
                    "effort",
                    "Reasoning",
                    defaults
                        .effort
                        .as_deref()
                        .map(effort_label)
                        .unwrap_or_else(|| "Model default".into()),
                    effort_menu(defaults, capabilities.as_ref()),
                    cx,
                ),
            )
            .child(self.picker_row(
                provider,
                "access",
                "Access",
                defaults.agent_access.unwrap_or_default().label().into(),
                access_menu(defaults),
                cx,
            ))
            .child(
                div()
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&theme))
                    .child(defaults.agent_access.unwrap_or_default().explanation()),
            );
        if capabilities
            .as_ref()
            .is_some_and(|model| model.supported_efforts.is_empty())
        {
            content = content.child(
                div()
                    .fx_caption()
                    .text_color(Colors::fg_tertiary(&theme))
                    .child("This model uses its provider’s reasoning settings."),
            );
        }
        card(content, Spacing::MD).into_any_element()
    }

    fn picker_row(
        &self,
        provider: &str,
        field: &str,
        title: &'static str,
        label: String,
        model: MenuModel,
        cx: &App,
    ) -> AnyElement {
        let theme = global::current();
        let owner = format!("{PREFIX}{provider}:{field}");
        let selector = owner.clone();
        let expanded = self.menu_layer.read(cx).is_owner(&owner);
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px_len(Spacing::MD))
            .child(
                div()
                    .flex_shrink_0()
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&theme))
                    .child(title),
            )
            .child(
                div()
                    .min_w_0()
                    .debug_selector(move || selector.clone())
                    .child(menu_trigger(
                        MenuTrigger::new(
                            model,
                            SharedString::from(owner),
                            SharedString::from(label),
                        )
                        .expanded(expanded),
                        &self.menu_layer,
                    )),
            )
            .into_any_element()
    }
}

impl Render for ProviderSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::MD))
            .child(section_header("Chat defaults"))
            .child(div().fx_caption().text_color(Colors::fg_secondary(&theme)).child("Choose the starting settings for new chats. Existing chats keep their own settings."))
            .child(self.provider_section("codex", "Codex", cx))
            .child(self.provider_section("claude", "Claude", cx))
    }
}

fn default_model_id(provider: &str) -> &'static str {
    if provider == "codex" {
        fx_core::DEFAULT_CODEX_MODEL_ID
    } else {
        "default"
    }
}

fn selected_model(provider: &str, defaults: &ProviderDefaults) -> Option<Model> {
    provider_catalog::model(
        provider,
        defaults
            .model_id
            .as_deref()
            .unwrap_or_else(|| default_model_id(provider)),
    )
}

fn selected_capabilities(provider: &str, defaults: &ProviderDefaults) -> Option<Model> {
    provider_catalog::known_model(
        provider,
        defaults
            .model_id
            .as_deref()
            .unwrap_or_else(|| default_model_id(provider)),
    )
}

fn model_label(provider: &str, defaults: &ProviderDefaults, model: Option<&Model>) -> String {
    defaults.model_id.as_ref().map_or_else(
        || {
            if provider == "codex" {
                format!(
                    "Default · {}",
                    model.map_or(default_model_id(provider), |model| model
                        .display_name
                        .as_str())
                )
            } else {
                "Provider default".into()
            }
        },
        |id| model.map_or_else(|| id.clone(), |model| model.display_name.clone()),
    )
}

fn model_menu(provider: &str, defaults: &ProviderDefaults, models: &[Model]) -> MenuModel {
    let mut items = vec![
        MenuItem::new(
            INHERIT,
            if provider == "codex" {
                "Default"
            } else {
                "Provider default"
            },
        )
        .subtitle(if provider == "codex" {
            default_model_id(provider)
        } else {
            "Use Claude’s default model"
        })
        .selected(defaults.model_id.is_none()),
    ];
    items.extend(
        models
            .iter()
            .filter(|model| model.id != "default")
            .map(|model| {
                MenuItem::new(&model.id, &model.display_name)
                    .selected(defaults.model_id.as_deref() == Some(&model.id))
            }),
    );
    if let Some(id) = &defaults.model_id
        && !models.iter().any(|model| model.id == *id)
    {
        items.push(
            MenuItem::new(id, id)
                .selected(true)
                .subtitle("Saved model; not in the current provider list"),
        );
    }
    MenuModel::new(vec![
        MenuSection::new("models", items),
        MenuSection::new("refresh", vec![MenuItem::new(REFRESH, "Refresh models")]),
    ])
}

fn effort_label(effort: &str) -> String {
    match effort {
        "xhigh" => "Extra high".into(),
        _ => {
            let mut chars = effort.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        }
    }
}

fn effort_menu(defaults: &ProviderDefaults, model: Option<&Model>) -> MenuModel {
    let mut items =
        vec![MenuItem::new(INHERIT, "Model default").selected(defaults.effort.is_none())];
    if let Some(model) = model {
        items.extend(model.supported_efforts.iter().map(|effort| {
            MenuItem::new(effort, effort_label(effort))
                .selected(defaults.effort.as_ref() == Some(effort))
        }));
    } else if let Some(effort) = &defaults.effort {
        items.push(
            MenuItem::new(effort, effort_label(effort))
                .selected(true)
                .subtitle("Saved reasoning; provider capabilities are unavailable"),
        );
    }
    MenuModel::new(vec![MenuSection::new("reasoning", items)])
}

fn access_menu(defaults: &ProviderDefaults) -> MenuModel {
    MenuModel::new(vec![MenuSection::new(
        "access",
        AgentAccess::ALL
            .into_iter()
            .map(|access| {
                MenuItem::new(access.label(), access.label())
                    .subtitle(access.explanation())
                    .selected(defaults.agent_access.unwrap_or_default() == access)
            })
            .collect(),
    )])
}

fn apply_choice(
    defaults: &mut ProviderDefaults,
    provider: &str,
    field: &str,
    chosen: &str,
) -> bool {
    match field {
        "model" => {
            if chosen != INHERIT
                && provider_catalog::model(provider, chosen).is_none()
                && defaults.model_id.as_deref() != Some(chosen)
            {
                return false;
            }
            defaults.model_id = (chosen != INHERIT).then(|| chosen.to_owned());
            let model = selected_capabilities(provider, defaults);
            if defaults.effort.as_ref().is_some_and(|effort| {
                model
                    .as_ref()
                    .is_some_and(|model| !model.supported_efforts.contains(effort))
            }) {
                defaults.effort = None;
            }
        }
        "effort" => {
            if chosen != INHERIT
                && selected_capabilities(provider, defaults).map_or_else(
                    || defaults.effort.as_deref() != Some(chosen),
                    |model| {
                        !model
                            .supported_efforts
                            .iter()
                            .any(|effort| effort == chosen)
                    },
                )
            {
                return false;
            }
            defaults.effort = (chosen != INHERIT).then(|| chosen.to_owned());
        }
        "access" => {
            let Some(access) = AgentAccess::ALL
                .into_iter()
                .find(|access| access.label() == chosen)
            else {
                return false;
            };
            defaults.agent_access = Some(access);
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_reasoning_survives_unknown_claude_capabilities_and_remains_selectable() {
        let mut defaults = ProviderDefaults {
            effort: Some("high".into()),
            ..Default::default()
        };
        assert!(selected_capabilities("claude", &defaults).is_none());
        let menu = effort_menu(&defaults, None);
        let selected = menu.sections()[0]
            .items
            .iter()
            .find(|item| item.selected)
            .unwrap();
        assert_eq!(selected.id, "high");
        assert!(selected.subtitle.is_some());
        assert!(apply_choice(&mut defaults, "claude", "model", INHERIT));
        assert_eq!(defaults.effort.as_deref(), Some("high"));
        assert!(apply_choice(&mut defaults, "claude", "effort", "high"));
        assert!(!apply_choice(
            &mut defaults,
            "claude",
            "effort",
            "unadvertised-choice"
        ));
        assert!(apply_choice(&mut defaults, "claude", "effort", INHERIT));
        assert!(defaults.effort.is_none());
    }

    fn reasoning_model(efforts: &[&str]) -> Model {
        Model {
            id: "example-model".into(),
            display_name: "Example model".into(),
            context_window: 0,
            max_context_window: 0,
            supported_efforts: efforts.iter().map(|effort| (*effort).into()).collect(),
            default_effort: efforts.first().map(|effort| (*effort).into()),
            supports_vision: false,
            is_default: false,
        }
    }

    #[test]
    fn reasoning_menu_only_offers_the_selected_models_capabilities() {
        let defaults = ProviderDefaults {
            effort: Some("high".into()),
            ..Default::default()
        };
        let menu = effort_menu(&defaults, Some(&reasoning_model(&["low", "high", "max"])));
        let items = &menu.sections()[0].items;
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            [INHERIT, "low", "high", "max"]
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| item.selected)
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["high"]
        );

        let menu = effort_menu(&ProviderDefaults::default(), Some(&reasoning_model(&[])));
        assert_eq!(menu.sections()[0].items.len(), 1);
        assert!(menu.sections()[0].items[0].selected);
    }

    #[test]
    fn unavailable_saved_models_remain_visible_until_a_replacement_is_chosen() {
        let defaults = ProviderDefaults {
            model_id: Some("custom-private-model".into()),
            ..Default::default()
        };
        let menu = model_menu("codex", &defaults, &[reasoning_model(&["low"])]);
        let selected = menu.sections()[0]
            .items
            .iter()
            .find(|item| item.selected)
            .unwrap();
        assert_eq!(selected.id, "custom-private-model");
        assert!(selected.subtitle.is_some());
    }

    #[test]
    fn changing_models_clears_unsupported_reasoning_and_rejects_unknown_choices() {
        let mut defaults = ProviderDefaults {
            effort: Some("unsupported-effort".into()),
            ..Default::default()
        };
        assert!(apply_choice(&mut defaults, "codex", "model", INHERIT));
        assert_eq!(defaults.effort, None);
        assert!(!apply_choice(
            &mut defaults,
            "codex",
            "effort",
            "unsupported-effort"
        ));
        assert!(!apply_choice(
            &mut defaults,
            "codex",
            "model",
            "unknown-model-for-test"
        ));
        assert_eq!(defaults, ProviderDefaults::default());
    }

    #[test]
    fn access_choices_keep_their_explanations_and_the_existing_default() {
        let menu = access_menu(&ProviderDefaults::default());
        let items = &menu.sections()[0].items;
        assert_eq!(items.len(), AgentAccess::ALL.len());
        for (item, access) in items.iter().zip(AgentAccess::ALL) {
            assert_eq!(item.subtitle.as_deref(), Some(access.explanation()));
            assert_eq!(item.selected, access == AgentAccess::FullAccess);
        }
    }
}

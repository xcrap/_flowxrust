//! Mounted workspace dialogs and their integration with the running sessions.

use std::sync::mpsc;

use fx_agent::provider::{UserInputRequest, UserResponse};
use fx_core::services::runtime::{BinaryHealth, BinarySpec, RuntimeDiscovery, all_specs};
use fx_design::components::{ButtonStyle, button, choice_row, divider, panel};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, IconSize, Layout, Shadow, Spacing, global};
use gpui::prelude::*;
use gpui::{App, Context, Entity, SharedString, Subscription, Window, div, px};
use uuid::Uuid;

use crate::settings::{self, ProviderReport, SettingsDraft, SettingsSheet};
use crate::state::AppState;
use crate::text_field::TextField;

type HealthResults = Vec<(BinarySpec, BinaryHealth)>;

#[derive(Clone)]
pub struct UiHandle(pub Entity<WorkspaceUi>);
impl gpui::Global for UiHandle {}

#[derive(Default)]
pub struct WorkspaceUi {
    pub settings: Option<Entity<SettingsSheet>>,
    pub request: Option<Entity<RequestPanel>>,
    pub thread_dialog: Option<Entity<crate::thread_actions::ThreadDialog>>,
    request_key: Option<(Uuid, String)>,
    health: HealthResults,
    health_results: Option<mpsc::Receiver<Result<HealthResults, String>>>,
    health_error: Option<String>,
}

pub fn handle(cx: &mut App) -> Entity<WorkspaceUi> {
    if let Some(handle) = cx.try_global::<UiHandle>() {
        return handle.0.clone();
    }
    let ui = cx.new(|_| WorkspaceUi::default());
    cx.set_global(UiHandle(ui.clone()));
    ui
}

pub fn has_modal(state: &Entity<AppState>, cx: &App) -> bool {
    state.read(cx).picker.is_some()
        || state.read(cx).palette.is_some()
        || cx.try_global::<UiHandle>().is_some_and(|handle| {
            let ui = handle.0.read(cx);
            ui.settings.is_some() || ui.thread_dialog.is_some() || ui.request.is_some()
        })
}

pub fn restore_preferences(state: &Entity<AppState>, cx: &mut App) {
    let current = global::current();
    let draft = preferences_draft(&state.read(cx).preferences, &current);
    global::set(draft.apply_to(&current));
}

fn preferences_draft(
    saved: &fx_store::PersistedPreferences,
    current: &fx_design::theme::Theme,
) -> SettingsDraft {
    fn stored_choice<T: Copy>(choices: Vec<settings::Choice<T>>, saved: &str) -> Option<T> {
        choices
            .into_iter()
            .find(|choice| choice.label.eq_ignore_ascii_case(saved.trim()))
            .map(|choice| choice.value)
    }
    SettingsDraft {
        appearance: stored_choice(settings::appearance_choices(), &saved.appearance)
            .unwrap_or(current.appearance_mode),
        base_tone: stored_choice(settings::tone_choices(), &saved.base_tone)
            .unwrap_or(current.base_tone),
        accent: stored_choice(settings::accent_choices(), &saved.accent)
            .unwrap_or(current.accent_color),
        text_size: stored_choice(settings::text_size_choices(), &saved.text_size)
            .unwrap_or(current.text_size),
    }
}

fn apply_settings(
    state: &Entity<AppState>,
    ui: &Entity<WorkspaceUi>,
    draft: SettingsDraft,
    baseline: SettingsDraft,
    defaults: (fx_store::ProviderDefaults, fx_store::ProviderDefaults),
    auto_titles: fx_store::AutoTitlePreferences,
    cx: &mut App,
) {
    let saved = fx_store::PersistedPreferences {
        appearance: draft.appearance.label().to_ascii_lowercase(),
        base_tone: draft.base_tone.label().to_ascii_lowercase(),
        accent: draft.accent.label().to_ascii_lowercase(),
        text_size: draft.text_size.label().to_ascii_lowercase(),
        codex_defaults: defaults.0,
        claude_defaults: defaults.1,
        auto_titles,
        follow_up_mode: state.read(cx).preferences.follow_up_mode.clone(),
    };
    match state.read(cx).store.save_preferences(&saved) {
        Err(error) => {
            // The primary file commits before the recovery copy. If only the
            // backup failed, keep this session consistent with the next launch.
            let committed = state
                .read(cx)
                .store
                .load_preferences_primary()
                .is_ok_and(|primary| primary == saved);
            if committed {
                state.update(cx, |state, cx| {
                    if state.preferences.auto_titles != saved.auto_titles {
                        state.cancel_title_jobs();
                    }
                    state.preferences = saved;
                    state.last_error = Some(format!(
                        "Settings were saved, but the recovery copy could not be updated: {error}"
                    ));
                    cx.notify();
                });
                global::set(draft.apply_to(&global::current()));
            } else {
                global::set(baseline.apply_to(&global::current()));
                state.update(cx, |state, cx| {
                    state.last_error = Some(format!("Could not save settings: {error}"));
                    cx.notify();
                });
            }
        }
        Ok(()) => {
            state.update(cx, |state, cx| {
                if state.preferences.auto_titles != saved.auto_titles {
                    state.cancel_title_jobs();
                }
                state.preferences = saved;
                cx.notify();
            });
            global::set(draft.apply_to(&global::current()));
        }
    }
    // Confirm settles the sheet before its callback. Remove it even on a
    // failed save, so an invisible settled modal cannot block the workspace.
    ui.update(cx, |ui, cx| {
        ui.settings = None;
        cx.notify();
    });
    cx.refresh_windows();
}

pub fn open_settings(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    if has_modal(&state, cx) {
        return;
    }
    dismiss_popup(&state, cx);
    // Catalog discovery is nonblocking and respects its refresh cooldown.
    // Tests exercise choices directly without launching installed provider CLIs.
    #[cfg(not(test))]
    for provider in ["codex", "claude"] {
        crate::provider_catalog::refresh(provider);
    }
    let ui = handle(cx);
    let apply_ui = ui.clone();
    let dismiss_ui = ui.clone();
    let refresh_ui = ui.clone();
    let archives = crate::archived_chats::ArchivedChats::new(state.clone(), cx);
    let preferences = state.read(cx).preferences.clone();
    let defaults = crate::provider_settings::ProviderSettings::new(
        preferences.codex_defaults,
        preferences.claude_defaults,
        state.read(cx).menu_layer.clone(),
        cx,
    );
    let apply_defaults = defaults.clone();
    let title_settings = crate::title_settings::TitleSettings::new(
        preferences.auto_titles,
        state.read(cx).menu_layer.clone(),
        cx,
    );
    let apply_titles = title_settings.clone();
    let follow_up =
        crate::follow_up_settings::FollowUpSettings::new(state.read(cx).follow_up_mode(), cx);
    let apply_follow_up = follow_up.clone();
    let baseline = SettingsDraft::from_theme(&global::current());
    let sheet = SettingsSheet::open(
        window,
        cx,
        Box::new(move |draft, window, cx| {
            let defaults = apply_defaults.read(cx).values();
            let auto_titles = apply_titles.read(cx).values();
            let follow_up = apply_follow_up.read(cx).value();
            apply_settings(
                &state,
                &apply_ui,
                draft,
                baseline,
                defaults,
                auto_titles,
                cx,
            );
            save_follow_up(&state, follow_up, cx);
            window.blur();
        }),
        Box::new(move |window, cx| {
            dismiss_ui.update(cx, |ui, cx| {
                ui.settings = None;
                cx.notify();
            });
            window.blur();
        }),
        Box::new(move |_, cx| {
            let refresh_ui = refresh_ui.clone();
            cx.defer(move |cx| refresh_ui.update(cx, |ui, cx| ui.recheck(cx)));
        }),
    );
    sheet.update(cx, |sheet, cx| {
        sheet.set_archives(archives);
        sheet.set_provider_defaults(defaults, cx);
        sheet.set_title_settings(title_settings, cx);
        sheet.set_follow_up_settings(follow_up, cx);
    });
    ui.update(cx, |ui, cx| {
        ui.settings = Some(sheet);
        ui.recheck(cx);
    });
}

/// Persist the "While responding" choice, when it changed.
///
/// Saved after the rest of the sheet rather than threaded through
/// `apply_settings`, which every settings test calls with its own arguments.
fn save_follow_up(state: &Entity<AppState>, mode: crate::state::FollowUp, cx: &mut App) {
    if state.read(cx).follow_up_mode() == mode {
        return;
    }
    state.update(cx, |state, cx| {
        let mut saved = state.preferences.clone();
        saved.follow_up_mode = mode.preference().into();
        match state.store.save_preferences(&saved) {
            Ok(()) => state.preferences = saved,
            Err(error) => state.last_error = Some(format!("Could not save settings: {error}")),
        }
        cx.notify();
    });
}

fn dismiss_popup(state: &Entity<AppState>, cx: &mut App) {
    let menu = state.read(cx).menu_layer.clone();
    menu.update(cx, |menu, cx| {
        if menu.dismiss().is_some() {
            cx.notify();
        }
    });
}

impl WorkspaceUi {
    fn recheck(&mut self, cx: &mut Context<Self>) {
        if self.health_results.is_some() {
            self.update_health_sheet(cx);
            cx.notify();
            return;
        }
        self.health_error = None;
        self.health = all_specs()
            .into_iter()
            .map(|spec| (spec, BinaryHealth::Checking))
            .collect();
        self.update_health_sheet(cx);
        let (tx, rx) = mpsc::channel();
        self.health_results = Some(rx);
        if let Err(error) = std::thread::Builder::new()
            .name("flowx-runtime-health".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = tx.send(Err(format!(
                            "Could not initialize the runtime check: {error}"
                        )));
                        return;
                    }
                };
                let results = runtime.block_on(async {
                    let discovery = RuntimeDiscovery::default();
                    let mut results = Vec::new();
                    for spec in all_specs() {
                        let health = discovery.health(&spec).await;
                        results.push((spec, health));
                    }
                    results
                });
                let _ = tx.send(Ok(results));
            })
        {
            tracing::warn!(%error, "could not start runtime health check");
            self.health_failed(format!("Could not start the runtime check: {error}"), cx);
        }
        cx.notify();
    }

    fn update_health_sheet(&self, cx: &mut App) {
        if let Some(sheet) = &self.settings {
            if let Some(error) = &self.health_error {
                sheet.update(cx, |sheet, cx| sheet.set_health_error(cx, error));
                return;
            }
            let reports: Vec<_> = self
                .health
                .iter()
                .map(|(spec, health)| ProviderReport::new(spec, health))
                .collect();
            sheet.update(cx, |sheet, cx| sheet.set_health(cx, &reports));
        }
    }

    pub fn is_checking(&self) -> bool {
        self.health_results.is_some()
    }

    fn health_failed(&mut self, reason: String, cx: &mut Context<Self>) {
        self.health_results = None;
        self.health_error = Some(reason);
        self.update_health_sheet(cx);
        cx.notify();
    }

    pub fn poll(&mut self, cx: &mut Context<Self>) {
        let result = self.health_results.as_ref().map(|rx| rx.try_recv());
        match result {
            Some(Ok(Ok(health))) => {
                self.health = health;
                self.health_results = None;
                self.update_health_sheet(cx);
                cx.notify();
            }
            Some(Ok(Err(reason))) => self.health_failed(reason, cx),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.health_failed(
                    "The runtime check stopped before it returned a result. Try Check again."
                        .into(),
                    cx,
                );
            }
            _ => {}
        }
    }

    pub fn sync_request(
        &mut self,
        state: &Entity<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A provider may ask while another thread's dialog owns focus. Defer
        // that request until the reader closes the current dialog.
        if self.settings.is_some()
            || self.thread_dialog.is_some()
            || state.read(cx).picker.is_some()
        {
            return;
        }
        let pending = state.read(cx).next_question();
        let key = pending
            .as_ref()
            .map(|(agent, request)| (*agent, request.id.clone()));
        if key == self.request_key {
            return;
        }
        if self.request.is_some() {
            window.blur();
        }
        if pending.is_some() {
            dismiss_popup(state, cx);
        }
        self.request_key = key;
        self.request =
            pending.map(|(agent, request)| RequestPanel::new(state.clone(), agent, request, cx));
    }
}

/// The dialog for a provider's question. Approvals are answered on their card
/// in the transcript instead.
pub struct RequestPanel {
    state: Entity<AppState>,
    agent: Uuid,
    request: UserInputRequest,
    fields: Vec<Entity<TextField>>,
    _observations: Vec<Subscription>,
    sent: bool,
    focus: gpui::FocusHandle,
    focused: bool,
}

impl RequestPanel {
    fn new(
        state: Entity<AppState>,
        agent: Uuid,
        request: UserInputRequest,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let fields: Vec<_> = request
                .questions
                .iter()
                .map(|question| {
                    cx.new(|cx| {
                        TextField::new(cx)
                            .placeholder("Your answer")
                            .max_len(16_000)
                            .secret(question.is_secret)
                    })
                })
                .collect();
            let observations = fields
                .iter()
                .map(|field| cx.observe(field, |_, _, cx| cx.notify()))
                .collect();
            Self {
                state,
                agent,
                request,
                fields,
                _observations: observations,
                sent: false,
                focus: cx.focus_handle(),
                focused: false,
            }
        })
    }

    fn respond(&mut self, response: UserResponse, cx: &mut Context<Self>) {
        if self.sent {
            return;
        }
        self.sent = self.state.update(cx, |state, cx| {
            let sent = state.respond_to_interaction(self.agent, response);
            cx.notify();
            sent
        });
        cx.notify();
    }

    fn answers(&self, cx: &App) -> Option<UserResponse> {
        let request = &self.request;
        let answers: Vec<_> = request
            .questions
            .iter()
            .zip(&self.fields)
            .map(|(question, field)| {
                let value = field.read(cx).value();
                (
                    question.id.clone(),
                    if question.is_secret {
                        value.to_string()
                    } else {
                        value.trim().to_string()
                    },
                )
            })
            .collect();
        if answers.len() != request.questions.len()
            || answers.is_empty()
            || answers.iter().any(|(_, value)| value.trim().is_empty())
        {
            return None;
        }
        Some(UserResponse::Questions {
            id: request.id.clone(),
            answers,
        })
    }
}

impl Render for RequestPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            if let Some(field) = self.fields.first() {
                field.read(cx).focus_handle().focus(window);
            } else {
                self.focus.focus(window);
            }
            self.focused = true;
        }
        let theme = global::current();
        let title = self
            .state
            .read(cx)
            .snapshot
            .projects
            .iter()
            .flat_map(|p| &p.agents)
            .find(|agent| agent.id == self.agent)
            .map(|agent| agent.title.clone())
            .unwrap_or_else(|| "Conversation".into());
        let request = &self.request;
        let heading = if request.questions.len() > 1 {
            "A few questions"
        } else {
            "A quick question"
        };
        let icon = Icon::QUESTIONMARK_BUBBLE_FILL;
        let header = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px_len(Spacing::MD))
            .px(px_len(Spacing::XXL))
            .py(px_len(Spacing::XL))
            .flex_shrink_0()
            .child(icon.render(IconSize::Large, Colors::fg_secondary(&theme), &theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px_len(Spacing::XXS))
                    .child(div().fx_title2().child(heading))
                    .child(
                        div()
                            .fx_caption()
                            .text_color(Colors::fg_secondary(&theme))
                            .child(SharedString::from(title)),
                    ),
            );
        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::XL))
            .p(px_len(Spacing::XXL));
        let mut footer = div()
            .w_full()
            .flex()
            .items_center()
            .justify_end()
            .gap(px_len(Spacing::SM))
            .px(px_len(Spacing::XXL))
            .py(px_len(Spacing::LG))
            .flex_shrink_0();
        for (index, question) in request.questions.iter().enumerate() {
            // An id per question, because a choice row's id is its label: two
            // questions offering "Yes" shared one row state, and a click on
            // the second question's never arrived.
            let mut section = div()
                .id(SharedString::from(format!("question:{index}")))
                .w_full()
                .flex()
                .flex_col()
                .gap(px_len(Spacing::MD));
            if request.questions.len() > 1 || !question.header.is_empty() {
                section = section.child(
                    div()
                        .fx_caption()
                        .text_color(Colors::fg_tertiary(&theme))
                        .child(SharedString::from(if question.header.is_empty() {
                            format!("Question {}", index + 1)
                        } else {
                            question.header.clone()
                        })),
                );
            }
            section = section.child(
                div()
                    .fx_body_medium()
                    .child(SharedString::from(question.question.clone())),
            );
            for option in &question.options {
                let field = self.fields[index].clone();
                let value = option.label.clone();
                let selected = field.read(cx).value() == option.label;
                let selector = format!("question-option:{index}:{}", option.label);
                section = section.child(
                    choice_row(
                        SharedString::from(option.label.clone()),
                        SharedString::from(option.description.clone()),
                        selected,
                        !self.sent,
                        move |_, _, cx| {
                            field.update(cx, |field, cx| field.set_value(value.clone(), cx))
                        },
                    )
                    .debug_selector(move || selector),
                );
            }
            section = section
                .when(!question.options.is_empty(), |section| {
                    section.child(
                        div()
                            .fx_caption()
                            .text_color(Colors::fg_secondary(&theme))
                            .child("Choose an option above or enter your answer."),
                    )
                })
                .child(self.fields[index].clone());
            body = body.child(section);
            if index + 1 < request.questions.len() {
                body = body.child(divider());
            }
        }
        let answers = self.answers(cx);
        let cancel = UserResponse::CancelQuestions {
            id: request.id.clone(),
        };
        footer = footer
            .child(button(
                "Cancel",
                ButtonStyle::Secondary,
                !self.sent,
                cx.listener(move |this, _, _, cx| this.respond(cancel.clone(), cx)),
            ))
            .child(button(
                "Send answers",
                ButtonStyle::Primary,
                !self.sent && answers.is_some(),
                cx.listener(move |this, _, _, cx| {
                    if let Some(answer) = this.answers(cx) {
                        this.respond(answer, cx);
                    }
                }),
            ));
        let content = div()
            .w_full()
            .flex()
            .flex_col()
            .child(header)
            .child(divider())
            .child(
                div()
                    .id("provider-request-content")
                    .w_full()
                    .min_h_0()
                    .max_h(
                        (window.viewport_size().height - px(Spacing::HUGE * 4.0))
                            .max(px(Spacing::HUGE * 2.0)),
                    )
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(divider())
            .child(footer);
        div()
            .id("provider-request-overlay")
            .track_focus(&self.focus)
            .occlude()
            .absolute()
            .inset_0()
            .bg(Colors::overlay(&theme))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px_len(Layout::SETTINGS_PANEL_WIDTH))
                    .max_w_full()
                    .p(px_len(Spacing::SM))
                    .fx_shadow(Shadow::dialog(Colors::overlay(&theme)))
                    .child(panel(content)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_agent::provider::{ProviderEvent, Question};
    use fx_design::theme::{AccentColor, AppearanceMode, BaseTone, TextSizePreset};
    use fx_store::{AppPaths, PersistedPreferences};

    fn state(cx: &mut App, path: &std::path::Path) -> Entity<AppState> {
        cx.new(|cx| AppState::restore(AppPaths::at(path), cx))
    }

    fn question(id: &str, secret: bool) -> Question {
        Question {
            id: id.into(),
            header: id.into(),
            question: format!("Answer {id}"),
            options: vec![],
            is_other: true,
            is_secret: secret,
        }
    }

    /// Render the real composer and modal entities together. A direct call to
    /// TextField::set_value cannot detect which platform input handler owns keys.
    struct MountedWorkspace {
        state: Entity<AppState>,
        ui: Entity<WorkspaceUi>,
        composer: Entity<TextField>,
    }

    impl Render for MountedWorkspace {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.state.update(cx, |state, _| state.drain_updates());
            self.ui
                .update(cx, |ui, cx| ui.sync_request(&self.state, window, cx));
            div()
                .relative()
                .size_full()
                .child(self.composer.clone())
                .children(self.ui.read(cx).settings.clone())
                .children(self.ui.read(cx).request.clone())
        }
    }

    #[test]
    fn a_mounted_secret_request_takes_input_away_from_the_covered_composer() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (state, ui, composer) =
            test.update(|cx| (state(cx, temp.path()), handle(cx), cx.new(TextField::new)));
        let window = test.add_window({
            let state = state.clone();
            let ui = ui.clone();
            let composer = composer.clone();
            move |window, cx| {
                composer.read(cx).focus_handle().focus(window);
                MountedWorkspace {
                    state,
                    ui,
                    composer,
                }
            }
        });
        test.refresh().expect("the composer painted");
        test.simulate_input(window.into(), "existing-draft");
        test.update(|cx| assert_eq!(composer.read(cx).value(), "existing-draft"));

        let agent = Uuid::new_v4();
        test.update(|cx| {
            state.read(cx).update_sender_for(agent)(crate::session::SessionUpdate::Interaction {
                event: ProviderEvent::UserInput(UserInputRequest {
                    id: "secret-request".into(),
                    questions: vec![question("password", true)],
                }),
            })
        });
        window.update(&mut test, |_, _, cx| cx.notify()).unwrap();
        test.refresh().expect("the question overlay painted");
        let field =
            test.update(|cx| ui.read(cx).request.as_ref().unwrap().read(cx).fields[0].clone());
        window
            .update(&mut test, |_, window, cx| {
                assert!(field.read(cx).focus_handle().is_focused(window));
                assert!(!composer.read(cx).focus_handle().is_focused(window));
            })
            .unwrap();
        test.simulate_input(window.into(), "test-secret");
        test.update(|cx| {
            assert_eq!(field.read(cx).value(), "test-secret");
            assert_eq!(composer.read(cx).value(), "existing-draft");
        });

        test.update(|cx| {
            state.read(cx).update_sender_for(agent)(
                crate::session::SessionUpdate::InteractionResolved {
                    id: "secret-request".into(),
                },
            )
        });
        window.update(&mut test, |_, _, cx| cx.notify()).unwrap();
        test.refresh().expect("the resolved overlay was removed");
        window
            .update(&mut test, |_, window, cx| {
                assert!(ui.read(cx).request.is_none());
                assert!(!field.read(cx).focus_handle().is_focused(window));
            })
            .unwrap();
        test.simulate_input(window.into(), "after");
        test.update(|cx| {
            assert_eq!(
                field.read(cx).value(),
                "test-secret",
                "removed request retained the input handler"
            );
            assert_eq!(
                composer.read(cx).value(),
                "existing-draft",
                "blur restored input into the covered draft"
            );
        });
    }

    #[test]
    fn two_questions_offering_the_same_choices_each_take_their_own_click() {
        // A choice row's id is its label. With both questions' rows under one
        // id, the first question's row consumed the press meant for the second.
        use gpui::{MouseButton, VisualTestContext, px, size};
        let temp = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (state, ui, composer) =
            test.update(|cx| (state(cx, temp.path()), handle(cx), cx.new(TextField::new)));
        let window = test.add_window({
            let state = state.clone();
            let ui = ui.clone();
            move |_, _| MountedWorkspace {
                state,
                ui,
                composer,
            }
        });
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1100.0), px(900.0)));
        let yes_or_no = |id: &str| Question {
            options: ["Yes", "No"]
                .map(|label| fx_agent::provider::Option_ {
                    label: label.into(),
                    description: String::new(),
                })
                .to_vec(),
            ..question(id, false)
        };
        test.update(|cx| {
            state.read(cx).update_sender_for(Uuid::new_v4())(
                crate::session::SessionUpdate::Interaction {
                    event: ProviderEvent::UserInput(UserInputRequest {
                        id: "two-questions".into(),
                        questions: vec![yes_or_no("tests"), yes_or_no("docs")],
                    }),
                },
            )
        });
        window.update(&mut test, |_, _, cx| cx.notify()).unwrap();
        test.refresh().unwrap();
        let fields = test.update(|cx| {
            ui.read(cx)
                .request
                .as_ref()
                .unwrap()
                .read(cx)
                .fields
                .clone()
        });
        let values = |test: &gpui::TestAppContext| {
            test.read(|cx| {
                fields
                    .iter()
                    .map(|field| field.read(cx).value().to_string())
                    .collect::<Vec<_>>()
            })
        };

        for (selector, expected) in [
            ("question-option:1:No", ["", "No"]),
            ("question-option:0:Yes", ["Yes", "No"]),
        ] {
            let row = visual.debug_bounds(selector).unwrap().center();
            visual.simulate_mouse_down(row, MouseButton::Left, Default::default());
            visual.simulate_mouse_up(row, MouseButton::Left, Default::default());
            test.refresh().unwrap();
            assert_eq!(values(&test), expected, "after clicking {selector}");
        }
    }

    #[test]
    fn an_approval_opens_no_dialog_and_leaves_the_composer_typing() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (state, ui, composer) =
            test.update(|cx| (state(cx, temp.path()), handle(cx), cx.new(TextField::new)));
        let window = test.add_window({
            let state = state.clone();
            let ui = ui.clone();
            let composer = composer.clone();
            move |window, cx| {
                composer.read(cx).focus_handle().focus(window);
                MountedWorkspace {
                    state,
                    ui,
                    composer,
                }
            }
        });
        test.refresh().unwrap();
        test.simulate_input(window.into(), "draft");
        test.update(|cx| {
            state.read(cx).update_sender_for(Uuid::new_v4())(
                crate::session::SessionUpdate::Interaction {
                    event: ProviderEvent::Approval(fx_agent::provider::ApprovalRequest {
                        id: "approval".into(),
                        tool_name: "shell".into(),
                        input: serde_json::json!({"command":"cargo test"}),
                        risk: fx_agent::provider::RiskLevel::Moderate,
                    }),
                },
            );
        });
        window.update(&mut test, |_, _, cx| cx.notify()).unwrap();
        test.refresh().expect("the workspace repainted");
        window
            .update(&mut test, |_, window, cx| {
                // Answered on its card in the transcript, not in a dialog.
                assert!(ui.read(cx).request.is_none());
                assert!(!has_modal(&state, cx));
                assert!(composer.read(cx).focus_handle().is_focused(window));
            })
            .unwrap();
        test.simulate_input(window.into(), " more");
        test.update(|cx| assert_eq!(composer.read(cx).value(), "draft more"));
    }

    #[test]
    fn settings_recheck_defers_refresh_until_the_sheet_entity_borrow_is_released() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        // Holding this pending mock prevents open_settings/recheck from ever
        // launching discovery against the test machine's real provider CLIs.
        let (_health_sender, health_receiver) = mpsc::channel();
        let (state, ui, composer) = test.update(|cx| {
            let ui = handle(cx);
            ui.update(cx, |ui, _| {
                ui.health_results = Some(health_receiver);
                ui.health = vec![(
                    BinarySpec::new("fixture", "Fixture runtime", vec![]),
                    BinaryHealth::Checking,
                )];
            });
            (state(cx, temp.path()), ui, cx.new(TextField::new))
        });
        let window = test.add_window(move |_, _| MountedWorkspace {
            state,
            ui,
            composer,
        });
        window
            .update(&mut test, |host, window, cx| {
                open_settings(host.state.clone(), window, cx);
                cx.notify();
            })
            .unwrap();
        test.refresh()
            .expect("the production settings sheet painted");
        window
            .update(&mut test, |host, window, cx| {
                let sheet = host.ui.read(cx).settings.clone().unwrap();
                // Same entity borrow used by SettingsSheet's Re-check button. The
                // old synchronous callback tried to update this very entity again.
                sheet.update(cx, |sheet, cx| sheet.recheck(window, cx));
            })
            .expect("rechecking did not re-enter the borrowed settings entity");
        test.refresh()
            .expect("the deferred health update repainted");
        window
            .update(&mut test, |host, _, cx| {
                let ui = host.ui.read(cx);
                assert!(ui.is_checking());
                let rows = ui.settings.as_ref().unwrap().read(cx).rows();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].title, "Fixture runtime");
            })
            .unwrap();
    }

    #[test]
    fn grouped_questions_keep_enclosing_and_nested_ids_and_require_every_answer() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = state(cx, temp.path());
            let panel = RequestPanel::new(
                state,
                Uuid::new_v4(),
                UserInputRequest {
                    id: "request-envelope".into(),
                    questions: vec![question("database", false), question("region", false)],
                },
                cx,
            );
            assert!(panel.read(cx).answers(cx).is_none());
            let fields = panel.read(cx).fields.clone();
            fields[0].update(cx, |field, cx| field.set_value("  SQLite  ", cx));
            fields[1].update(cx, |field, cx| field.set_value("  \t ", cx));
            assert!(
                panel.read(cx).answers(cx).is_none(),
                "a whitespace-only answer must not complete a group"
            );
            fields[1].update(cx, |field, cx| field.set_value("Azores", cx));
            assert_eq!(
                panel.read(cx).answers(cx),
                Some(UserResponse::Questions {
                    id: "request-envelope".into(),
                    answers: vec![
                        ("database".into(), "SQLite".into()),
                        ("region".into(), "Azores".into())
                    ],
                })
            );
        });
    }

    #[test]
    fn secret_questions_use_masked_fields_and_preserve_the_exact_answer() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = state(cx, temp.path());
            let panel = RequestPanel::new(
                state,
                Uuid::new_v4(),
                UserInputRequest {
                    id: "auth-request".into(),
                    questions: vec![question("password", true)],
                },
                cx,
            );
            let field = panel.read(cx).fields[0].clone();
            assert!(field.read(cx).is_secret());
            field.update(cx, |field, cx| field.set_value("  test-secret-🜁  ", cx));
            assert_eq!(
                panel.read(cx).answers(cx),
                Some(UserResponse::Questions {
                    id: "auth-request".into(),
                    answers: vec![("password".into(), "  test-secret-🜁  ".into())],
                })
            );
        });
    }

    #[test]
    fn stored_preferences_accept_stable_lowercase_and_legacy_display_labels() {
        let current = crate::state::default_theme();
        let saved = PersistedPreferences {
            appearance: "light".into(),
            base_tone: "stone".into(),
            accent: "orange".into(),
            text_size: "large".into(),
            ..Default::default()
        };
        let expected = SettingsDraft {
            appearance: AppearanceMode::Light,
            base_tone: BaseTone::Stone,
            accent: AccentColor::Orange,
            text_size: TextSizePreset::Large,
        };
        assert_eq!(preferences_draft(&saved, &current), expected);
        let legacy = PersistedPreferences {
            appearance: "Light".into(),
            base_tone: "Stone".into(),
            accent: "Orange".into(),
            text_size: "Large".into(),
            ..Default::default()
        };
        assert_eq!(preferences_draft(&legacy, &current), expected);
        let invalid = PersistedPreferences {
            accent: "removed-accent".into(),
            ..saved
        };
        assert_eq!(
            preferences_draft(&invalid, &current).accent,
            current.accent_color
        );
    }

    #[test]
    fn settings_default_picker_routes_through_the_root_and_saves_only_on_done() {
        use gpui::{MouseButton, VisualTestContext, px, size};
        let temp = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (_health_sender, health_receiver) = mpsc::channel();
        let (state, ui) = test.update(|cx| {
            let ui = handle(cx);
            ui.update(cx, |ui, _| ui.health_results = Some(health_receiver));
            let state = state(cx, temp.path());
            crate::AppHandle::install(state.clone(), cx);
            (state, ui)
        });
        let for_view = state.clone();
        let window = test.add_window(move |_, _| crate::FlowX::new(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1280.0), px(900.0)));
        test.refresh().unwrap();
        window
            .update(&mut test, |_, window, cx| {
                open_settings(state.clone(), window, cx)
            })
            .unwrap();
        test.refresh().unwrap();
        test.update(|_| {});
        let picker = visual
            .debug_bounds("settings-defaults:codex:access")
            .unwrap()
            .center();
        visual.simulate_mouse_down(picker, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(picker, MouseButton::Left, Default::default());
        test.read(|cx| assert!(state.read(cx).menu_layer.read(cx).is_open()));
        visual.simulate_keystrokes("down enter");
        test.read(|cx| {
            assert!(!state.read(cx).menu_layer.read(cx).is_open());
            assert!(ui.read(cx).settings.is_some());
            let sheet = ui.read(cx).settings.clone().unwrap();
            let editor = sheet.read(cx).provider_defaults().unwrap();
            assert_eq!(
                editor.read(cx).values().0.agent_access,
                Some(fx_core::AgentAccess::Supervised)
            );
            assert_eq!(state.read(cx).preferences.codex_defaults.agent_access, None);
        });
        visual.simulate_keystrokes("enter");
        test.read(|cx| {
            assert!(ui.read(cx).settings.is_none());
            assert_eq!(
                state.read(cx).preferences.codex_defaults.agent_access,
                Some(fx_core::AgentAccess::Supervised)
            );
            assert_eq!(
                state
                    .read(cx)
                    .store
                    .load_preferences()
                    .unwrap()
                    .codex_defaults
                    .agent_access,
                Some(fx_core::AgentAccess::Supervised)
            );
        });
    }

    #[test]
    fn the_while_responding_choice_saves_on_done_and_not_on_escape() {
        use gpui::{VisualTestContext, px, size};
        let temp = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (_health_sender, health_receiver) = mpsc::channel();
        let state = test.update(|cx| {
            let ui = handle(cx);
            ui.update(cx, |ui, _| ui.health_results = Some(health_receiver));
            let state = state(cx, temp.path());
            crate::AppHandle::install(state.clone(), cx);
            state
        });
        let for_view = state.clone();
        let window = test.add_window(move |_, _| crate::FlowX::new(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1280.0), px(900.0)));
        test.refresh().unwrap();
        let choose_queue = |test: &mut gpui::TestAppContext| {
            window
                .update(test, |_, window, cx| {
                    open_settings(state.clone(), window, cx)
                })
                .unwrap();
            test.refresh().unwrap();
            test.update(|cx| {
                let sheet = handle(cx).read(cx).settings.clone().unwrap();
                let editor = sheet.read(cx).follow_up_settings().unwrap();
                editor.update(cx, |editor, cx| {
                    editor.set(crate::state::FollowUp::Queue, cx)
                });
            });
            test.update(|cx| {
                let sheet = handle(cx).read(cx).settings.clone().unwrap();
                assert!(sheet.read(cx).is_dirty(), "the choice is a change to apply");
            });
            test.refresh().unwrap();
        };

        choose_queue(&mut test);
        visual.simulate_keystrokes("escape");
        test.read(|cx| {
            assert_eq!(
                state.read(cx).follow_up_mode(),
                crate::state::FollowUp::Steer
            );
        });

        choose_queue(&mut test);
        visual.simulate_keystrokes("enter");
        test.read(|cx| {
            assert_eq!(
                state.read(cx).follow_up_mode(),
                crate::state::FollowUp::Queue
            );
            let saved = state.read(cx).store.load_preferences().unwrap();
            assert_eq!(saved.follow_up_mode, "queue");
        });
    }

    #[test]
    fn provider_defaults_save_with_appearance_and_restore_for_new_chats() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = state(cx, temp.path());
            let ui = cx.new(|_| WorkspaceUi::default());
            let appearance = SettingsDraft::from_theme(&global::current());
            let codex = fx_store::ProviderDefaults {
                model_id: Some(fx_core::DEFAULT_CODEX_MODEL_ID.into()),
                effort: Some("high".into()),
                agent_access: Some(fx_core::AgentAccess::Supervised),
            };
            let claude = fx_store::ProviderDefaults {
                model_id: Some("sonnet".into()),
                effort: Some("medium".into()),
                agent_access: Some(fx_core::AgentAccess::AcceptEdits),
            };
            apply_settings(
                &state,
                &ui,
                appearance,
                appearance,
                (codex.clone(), claude.clone()),
                state.read(cx).preferences.auto_titles.clone(),
                cx,
            );
            assert_eq!(state.read(cx).preferences.codex_defaults, codex);
            let restored = cx.new(|cx| AppState::restore(AppPaths::at(temp.path()), cx));
            assert_eq!(restored.read(cx).preferences.codex_defaults, codex);
            assert_eq!(restored.read(cx).preferences.claude_defaults, claude);
            assert_eq!(
                restored
                    .read(cx)
                    .default_configuration("claude")
                    .agent_access,
                Some(fx_core::AgentAccess::AcceptEdits)
            );
            apply_settings(
                &state,
                &ui,
                appearance,
                appearance,
                (codex.clone(), claude.clone()),
                state.read(cx).preferences.auto_titles.clone(),
                cx,
            );
            let saved = state.read(cx).store.load_preferences().unwrap();
            assert_eq!(saved.codex_defaults, codex);
            assert_eq!(saved.claude_defaults, claude);
        });
    }

    #[test]
    fn chat_names_inside_chat_defaults_select_and_persist_only_on_done() {
        use gpui::{MouseButton, VisualTestContext, px, size};
        let temp = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (_health_sender, health_receiver) = mpsc::channel();
        let (state, ui) = test.update(|cx| {
            let ui = handle(cx);
            ui.update(cx, |ui, _| ui.health_results = Some(health_receiver));
            let state = state(cx, temp.path());
            crate::AppHandle::install(state.clone(), cx);
            (state, ui)
        });
        let for_view = state.clone();
        let window = test.add_window(move |_, _| crate::FlowX::new(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1280.0), px(900.0)));
        test.refresh().unwrap();
        window
            .update(&mut test, |_, window, cx| {
                open_settings(state.clone(), window, cx)
            })
            .unwrap();
        test.refresh().unwrap();
        test.update(|_| {});
        assert!(visual.debug_bounds("settings-nav-chat-names").is_none());
        let defaults = visual.debug_bounds("settings-page-chat-defaults").unwrap();
        assert!(visual.debug_bounds("settings-chat-names").is_some());
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: defaults.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-1000.0))),
            ..Default::default()
        });
        test.refresh().unwrap();
        let picker = visual
            .debug_bounds("settings-titles:model")
            .unwrap()
            .center();
        visual.simulate_mouse_down(picker, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(picker, MouseButton::Left, Default::default());
        let expected_model = crate::provider_catalog::models("codex")[0].id.clone();
        visual.simulate_keystrokes("down enter");
        test.read(|cx| {
            let sheet = ui
                .read(cx)
                .settings
                .clone()
                .expect("picker selection must not submit settings");
            assert_eq!(
                sheet
                    .read(cx)
                    .title_settings()
                    .unwrap()
                    .read(cx)
                    .values()
                    .model_id,
                expected_model
            );
            assert_eq!(
                state.read(cx).preferences.auto_titles,
                fx_store::AutoTitlePreferences::default()
            );
        });
        let enabled = visual.debug_bounds("settings-titles-enabled").unwrap();
        let off = gpui::point(
            enabled.left() + enabled.size.width / 4.0,
            enabled.center().y,
        );
        visual.simulate_mouse_down(off, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(off, MouseButton::Left, Default::default());
        test.refresh().unwrap();
        test.read(|cx| {
            let editor = ui
                .read(cx)
                .settings
                .as_ref()
                .unwrap()
                .read(cx)
                .title_settings()
                .unwrap();
            assert!(!editor.read(cx).values().enabled);
            assert!(state.read(cx).preferences.auto_titles.enabled);
        });
        visual.simulate_keystrokes("enter");
        test.read(|cx| {
            assert!(ui.read(cx).settings.is_none());
            let saved = state.read(cx).store.load_preferences().unwrap();
            assert!(!saved.auto_titles.enabled);
            assert_eq!(saved.auto_titles.model_id, expected_model);
            assert_eq!(saved.auto_titles, state.read(cx).preferences.auto_titles);
            assert_eq!(
                saved.codex_defaults,
                state.read(cx).preferences.codex_defaults
            );
        });
        window
            .update(&mut test, |_, window, cx| {
                open_settings(state.clone(), window, cx)
            })
            .unwrap();
        test.refresh().unwrap();
        test.update(|cx| {
            let editor = ui
                .read(cx)
                .settings
                .as_ref()
                .unwrap()
                .read(cx)
                .title_settings()
                .unwrap();
            editor.update(cx, |editor, cx| editor.set_enabled(true, cx));
        });
        visual.simulate_keystrokes("escape");
        test.read(|cx| {
            assert!(ui.read(cx).settings.is_none());
            assert!(!state.read(cx).preferences.auto_titles.enabled);
            assert!(
                !state
                    .read(cx)
                    .store
                    .load_preferences()
                    .unwrap()
                    .auto_titles
                    .enabled
            );
        });
    }

    #[test]
    fn a_settings_backup_failure_keeps_committed_defaults_consistent_with_restart() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = state(cx, temp.path());
            let path = state.read(cx).paths.preferences_file();
            std::fs::create_dir(fx_store::write::backup_path(&path)).unwrap();
            let ui = cx.new(|_| WorkspaceUi::default());
            let before = global::current();
            let draft = SettingsDraft::from_theme(&before.with_accent(before.accent_color.next()));
            let defaults = fx_store::ProviderDefaults {
                effort: Some("high".into()),
                agent_access: Some(fx_core::AgentAccess::Supervised),
                ..Default::default()
            };
            apply_settings(
                &state,
                &ui,
                draft,
                SettingsDraft::from_theme(&before),
                (defaults.clone(), Default::default()),
                state.read(cx).preferences.auto_titles.clone(),
                cx,
            );
            assert_eq!(state.read(cx).preferences.codex_defaults, defaults);
            assert_eq!(
                state.read(cx).store.load_preferences().unwrap(),
                state.read(cx).preferences
            );
            let restored = cx.new(|cx| AppState::restore(AppPaths::at(temp.path()), cx));
            assert_eq!(restored.read(cx).preferences, state.read(cx).preferences);
            assert_eq!(global::current().accent_color, draft.accent);
            assert!(
                state
                    .read(cx)
                    .last_error
                    .as_deref()
                    .unwrap()
                    .starts_with("Settings were saved, but the recovery copy")
            );
            assert!(ui.read(cx).settings.is_none());
        });
    }

    #[test]
    fn a_settings_save_failure_closes_the_settled_sheet_and_reports_the_error() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = state(cx, temp.path());
            // A directory at the target is a deterministic write failure even for root.
            std::fs::create_dir(state.read(cx).paths.preferences_file()).unwrap();
            let ui = cx.new(|_| WorkspaceUi::default());
            let sheet = SettingsSheet::new(
                cx,
                Box::new(|_, _, _| {}),
                Box::new(|_, _| {}),
                Box::new(|_, _| {}),
            );
            ui.update(cx, |ui, _| ui.settings = Some(sheet));
            let before = global::current();
            let draft = SettingsDraft::from_theme(&before.with_accent(before.accent_color.next()));
            global::set(draft.apply_to(&before));
            let original = state.read(cx).preferences.clone();
            let changed_defaults = fx_store::ProviderDefaults {
                model_id: Some("unsaved-model".into()),
                agent_access: Some(fx_core::AgentAccess::Supervised),
                ..Default::default()
            };
            apply_settings(
                &state,
                &ui,
                draft,
                SettingsDraft::from_theme(&before),
                (changed_defaults, Default::default()),
                state.read(cx).preferences.auto_titles.clone(),
                cx,
            );
            assert_eq!(state.read(cx).preferences, original);
            assert!(ui.read(cx).settings.is_none());
            assert!(
                state
                    .read(cx)
                    .last_error
                    .as_deref()
                    .unwrap()
                    .starts_with("Could not save settings:")
            );
            assert_eq!(global::current().accent_color, before.accent_color);
        });
    }

    #[test]
    fn a_disconnected_health_worker_does_not_leave_permanent_checking_rows() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let sheet = SettingsSheet::new(
                cx,
                Box::new(|_, _, _| {}),
                Box::new(|_, _| {}),
                Box::new(|_, _| {}),
            );
            let ui = cx.new(|_| WorkspaceUi::default());
            ui.update(cx, |ui, cx| {
                ui.settings = Some(sheet.clone());
                ui.health = all_specs()
                    .into_iter()
                    .map(|spec| (spec, BinaryHealth::Checking))
                    .collect();
                ui.update_health_sheet(cx);
                let (sender, receiver) = mpsc::channel();
                ui.health_results = Some(receiver);
                drop(sender);
                ui.poll(cx);
                assert!(!ui.is_checking());
            });
            assert!(!sheet.read(cx).rows().is_empty());
            assert!(
                sheet
                    .read(cx)
                    .rows()
                    .iter()
                    .all(|row| row.badge == "Check failed" && row.tone == settings::RowTone::Bad)
            );
        });
    }
}

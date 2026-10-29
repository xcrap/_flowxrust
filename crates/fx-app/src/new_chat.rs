//! One creation flow for the keyboard, toolbar and project menu.

use crate::{AppState, text_field::TextField, workspace_ui};
use fx_core::AgentConfiguration;
use fx_design::components::{ButtonStyle, button, choice_row, divider, panel};
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, Layout, Spacing, global};
use gpui::{
    App, Context, Entity, FocusHandle, MouseButton, Render, Subscription, Task, Window, div,
    prelude::*,
};
use uuid::Uuid;

pub(crate) struct NewChat {
    state: Entity<AppState>,
    project: Uuid,
    root: String,
    id: Uuid,
    configuration: AgentConfiguration,
    worktree: bool,
    branch: Entity<TextField>,
    focus: FocusHandle,
    error: Option<String>,
    creating: bool,
    prepared: Option<fx_core::models::agent::ChatWorkspace>,
    task: Option<Task<()>>,
    _changed: Subscription,
}

pub(crate) fn open(
    state: Entity<AppState>,
    project: Option<Uuid>,
    window: &mut Window,
    cx: &mut App,
) {
    if workspace_ui::has_modal(&state, cx) {
        return;
    }
    let current = state.read(cx);
    let Some(project) = project
        .or(current.snapshot.active_project_id)
        .and_then(|id| current.project(id))
        .cloned()
    else {
        return;
    };
    let provider = current
        .active_agent()
        .and_then(|a| a.configuration.provider_id)
        .unwrap_or_else(|| "codex".into());
    let configuration = current.default_configuration(&provider);
    let id = Uuid::new_v4();
    let dialog = cx.new(|cx| {
        let branch = cx.new(|cx| {
            let mut field = TextField::new(cx).placeholder("Branch name").max_len(200);
            field.set_value(format!("flowx/{}", &id.simple().to_string()[..8]), cx);
            field
        });
        let changed = cx.observe(&branch, |_, _, cx| cx.notify());
        NewChat {
            state: state.clone(),
            project: project.id,
            root: project.root_path,
            id,
            configuration,
            worktree: false,
            branch,
            focus: cx.focus_handle(),
            error: None,
            creating: false,
            prepared: None,
            task: None,
            _changed: changed,
        }
    });
    dialog.read(cx).focus.focus(window);
    workspace_ui::handle(cx).update(cx, |ui, cx| {
        ui.new_chat = Some(dialog);
        cx.notify();
    });
    state.update(cx, |_, cx| cx.notify());
}

impl NewChat {
    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        workspace_ui::handle(cx).update(cx, |ui, cx| {
            ui.new_chat = None;
            cx.notify();
        });
        window.blur();
        self.state.update(cx, |_, cx| cx.notify());
    }

    fn choose(&mut self, worktree: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating || self.prepared.is_some() {
            return;
        }
        self.worktree = worktree;
        self.error = None;
        if worktree {
            self.branch.read(cx).focus_handle().focus(window);
        } else {
            self.focus.focus(window);
        }
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating || (self.worktree && self.branch.read(cx).is_composing()) {
            return;
        }
        self.error = None;
        if !self.worktree || self.prepared.is_some() {
            let result = self.state.update(cx, |state, cx| {
                state.save_all(cx);
                let result = state.create_workspace_thread(
                    self.project,
                    self.id,
                    self.configuration.clone(),
                    self.prepared.clone(),
                );
                cx.notify();
                result
            });
            match result {
                Ok(_) => self.close(window, cx),
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                }
            }
            return;
        }
        self.focus.focus(window);
        self.creating = true;
        let root = std::path::PathBuf::from(&self.root);
        let destination = self
            .state
            .read(cx)
            .store
            .paths()
            .root()
            .join("worktrees")
            .join(self.project.to_string())
            .join(self.id.to_string());
        let recovery = destination.display().to_string();
        let branch = self.branch.read(cx).value().to_string();
        let job = crate::inspector::repository::spawn_git(move || async move {
            fx_store::paths::create_private_dir(destination.parent().unwrap())
                .map_err(|e| e.to_string())?;
            fx_core::git::worktree::create(&root, &destination, &branch).await
        });
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = crate::inspector::repository::finish_git(job).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.creating = false;
                this.task = None;
                let result = result.and_then(|workspace| {
                    this.prepared = Some(workspace.clone());
                    this.state.update(cx, |state, cx| {
                        state.save_all(cx);
                        let result = state.create_workspace_thread(
                            this.project, this.id, this.configuration.clone(), Some(workspace),
                        );
                        cx.notify();
                        result
                    }).map_err(|error| format!("{error}\nThe checkout is kept at {recovery}. Retry saving this conversation."))
                });
                match result {
                    Ok(_) => this.close(window, cx),
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }
}

impl Render for NewChat {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let editable = !self.creating && self.prepared.is_none();
        let body = div()
            .flex().flex_col()
            .gap(px_len(Spacing::MD)).p(px_len(Spacing::XL))
            .child(div().fx_title3().text_color(Colors::fg(&theme)).child("New conversation"))
            .child(choice_row(
                "Current folder",
                "Use the project's existing checkout and changes.",
                !self.worktree, editable,
                cx.listener(|this, _, window, cx| this.choose(false, window, cx)),
            ))
            .child(choice_row(
                "New worktree",
                "Create a separate branch from the latest commit. Uncommitted changes stay in the original folder.",
                self.worktree, editable,
                cx.listener(|this, _, window, cx| this.choose(true, window, cx)),
            ))
            .children(self.worktree.then(|| {
                let branch = if editable {
                    self.branch.clone().into_any_element()
                } else {
                    div().fx_body().child(self.branch.read(cx).value().to_string()).into_any_element()
                };
                div().flex().flex_col().gap(px_len(Spacing::XS))
                    .child(div().fx_caption().text_color(Colors::fg_secondary(&theme)).child("New branch"))
                    .child(branch)
                    .child(div().fx_caption().text_color(Colors::fg_secondary(&theme))
                        .child("Archiving or deleting a chat keeps its worktree and branch."))
            }))
            .children(self.error.clone().map(|error| {
                div().fx_caption().text_color(Colors::fg(&theme)).child(error)
            }));
        let content = div().flex().flex_col().child(body).child(divider()).child(
            div()
                .flex()
                .justify_end()
                .gap(px_len(Spacing::SM))
                .px(px_len(Spacing::XL))
                .py(px_len(Spacing::MD))
                .child(button(
                    "Cancel",
                    ButtonStyle::Secondary,
                    !self.creating,
                    cx.listener(|this, _, window, cx| this.close(window, cx)),
                ))
                .child(button(
                    if self.creating {
                        "Creating worktree…"
                    } else if self.prepared.is_some() {
                        "Save conversation"
                    } else {
                        "Create conversation"
                    },
                    ButtonStyle::Primary,
                    !self.creating
                        && (!self.worktree || !self.branch.read(cx).value().trim().is_empty()),
                    cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                )),
        );
        div()
            .id("new-chat-dialog")
            .debug_selector(|| "new-chat-dialog".into())
            .absolute()
            .size_full()
            .occlude()
            .track_focus(&self.focus)
            .bg(Colors::overlay(&theme))
            .flex()
            .items_center()
            .justify_center()
            .p(px_len(Spacing::XL))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => {
                        this.close(window, cx);
                        cx.stop_propagation();
                    }
                    "enter" => {
                        this.confirm(window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .w(px_len(Layout::DIALOG_WIDTH))
                    .max_w_full()
                    .child(panel(content)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlowX;
    use fx_store::AppPaths;
    use std::path::Path;

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new(fx_core::git::GIT)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(root)
            .envs(fx_core::git::hardened_environment())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn creating_switching_reopening_and_deleting_a_worktree_chat_preserves_both_checkouts() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["config", "user.name", "FlowX test"]);
        git(&source, &["config", "user.email", "qa@example.invalid"]);
        std::fs::write(source.join("sample"), "committed").unwrap();
        git(&source, &["add", "sample"]);
        git(&source, &["commit", "-qm", "Initial"]);
        std::fs::write(source.join("sample"), "unsaved work").unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let mut test = gpui::TestAppContext::single();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        let original = state.update(&mut test, |state, _| {
            state.add_project(source.to_str().unwrap());
            state.create_thread().unwrap()
        });
        let window = test.add_window({
            let state = state.clone();
            move |_, _| FlowX::new(state)
        });
        window
            .update(&mut test, |_, window, cx| {
                open(state.clone(), None, window, cx);
                let dialog = workspace_ui::handle(cx).read(cx).new_chat.clone().unwrap();
                dialog.update(cx, |dialog, cx| {
                    dialog.choose(true, window, cx);
                    dialog
                        .branch
                        .update(cx, |field, cx| field.set_value("flowx/isolated", cx));
                    dialog.confirm(window, cx);
                });
            })
            .unwrap();
        for _ in 0..1000 {
            test.run_until_parked();
            if test.update(|cx| workspace_ui::handle(cx).read(cx).new_chat.is_none()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        test.update(|cx| {
            if let Some(dialog) = &workspace_ui::handle(cx).read(cx).new_chat {
                panic!("creation did not finish: {:?}", dialog.read(cx).error);
            }
        });
        let (id, workspace) = test.read(|cx| {
            let state = state.read(cx);
            let agent = state.active_agent().unwrap();
            let workspace = agent.workspace.unwrap();
            assert_eq!(
                state.active_workspace_root().as_deref(),
                Some(workspace.path.as_str())
            );
            assert_eq!(state.recovery_target(agent.id).unwrap().1, workspace.path);
            (agent.id, workspace)
        });
        assert_eq!(
            std::fs::read_to_string(Path::new(&workspace.path).join("sample")).unwrap(),
            "committed"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("sample")).unwrap(),
            "unsaved work"
        );
        let monitor = test.update(|cx| crate::inspector::monitor::shared(&state, cx));
        state.update(&mut test, |state, _| state.select_agent(original));
        monitor.update(&mut test, |monitor, cx| {
            monitor.sync(cx);
            assert_eq!(monitor.project(), Some(source.to_str().unwrap()));
        });
        state.update(&mut test, |state, _| state.select_agent(id));
        monitor.update(&mut test, |monitor, cx| {
            monitor.sync(cx);
            assert_eq!(monitor.project(), Some(workspace.path.as_str()));
            assert!(
                monitor.snapshot().is_none(),
                "old checkout's snapshot must be cleared"
            );
        });
        state.update(&mut test, |state, cx| {
            state.save_all(cx);
            state.flush_saves();
        });
        let restored = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        restored.update(&mut test, |state, _| {
            assert_eq!(
                state.thread(id).unwrap().workspace.as_ref(),
                Some(&workspace)
            );
            assert_eq!(
                state.project_root_for(original).as_deref(),
                Some(source.to_str().unwrap())
            );
            assert!(state.delete_thread(id));
        });
        assert!(Path::new(&workspace.path).join(".git").is_file());
        assert_eq!(
            std::fs::read_to_string(source.join("sample")).unwrap(),
            "unsaved work"
        );
    }
}

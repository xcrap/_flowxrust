//! Committing and pushing from the inspector.
//!
//! The composer is a message, a choice of what to include, and a Commit
//! button. Push lives in the inspector header because it is available whether
//! or not anything is left to commit. Both run on the Git worker, never on the
//! UI thread, and both report git's own words when they fail: a hook's
//! explanation or a rejected ref's hint is what the user acts on.

use std::collections::{BTreeSet, HashMap};

use fx_core::GitRepositoryInfo;
use fx_core::git::{CommitPlan, CommitScope};
use fx_design::components::{
    ButtonStyle, IconButtonProps, SegmentedOption, button_with_icon, checkbox,
    compact_action_button, icon_button, segmented_control,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, IconSize, Layout, Spacing, TextRole, global};
use gpui::prelude::*;
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, SharedString, Subscription, Task, Window, div, px,
};

use super::Inspector;
use super::repository::{self, ComparisonMode};
use crate::text_field::TextField;

/// What the user chose to include.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommitChoice {
    /// Stage everything, new files included.
    All,
    /// The index as it stands.
    Staged,
    /// Only the files ticked in the list.
    Chosen,
}

/// The write in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Commit,
    Push,
}

impl Operation {
    fn progress(self) -> &'static str {
        match self {
            Self::Commit => "Committing…",
            Self::Push => "Pushing…",
        }
    }

    fn failure(self) -> &'static str {
        match self {
            Self::Commit => "Commit failed",
            Self::Push => "Push failed",
        }
    }
}

/// How the last write ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Succeeded(String),
    Failed(Operation, String),
}

pub(super) struct CommitComposer {
    pub(super) visible: bool,
    pub(super) field: Entity<TextField>,
    pub(super) choice: CommitChoice,
    pub(super) chosen: BTreeSet<String>,
    /// How the last write in the project shown ended.
    pub(super) outcome: Option<Outcome>,
    /// Unsent messages, by project, so switching projects does not lose one.
    drafts: HashMap<String, String>,
    /// Writes in flight, by the project they write to.
    ///
    /// A switch keeps them. Dropping the task cancelled it, and git went on
    /// to commit without anyone hearing: back in its project the committed
    /// message sat in the field as a draft, with no banner, and Commit was
    /// enabled again, so a second press made a duplicate commit.
    writes: HashMap<String, Write>,
    /// How writes ended in a project that was not shown then, until it is.
    unseen: HashMap<String, Outcome>,
    _field_observation: Subscription,
}

/// A write in flight, and the task that reports how it ends.
struct Write {
    operation: Operation,
    _task: Task<()>,
}

impl CommitComposer {
    pub(super) fn new(cx: &mut Context<Inspector>) -> Self {
        let inspector = cx.weak_entity();
        let field = cx.new(|cx| {
            TextField::new(cx)
                .placeholder("Commit message · ⌘↩ to commit")
                .multiline(4.0)
                .max_len(10_000)
                .on_submit(move |message, cx| {
                    // The field is mid-update here, so the message travels as
                    // an argument rather than being read back from it.
                    let _ = inspector.update(cx, |inspector, cx| inspector.commit(message, cx));
                })
        });
        // The Commit button's enabled state follows the message.
        let observation = cx.observe(&field, |_, _, cx| cx.notify());
        Self {
            visible: false,
            field,
            choice: CommitChoice::All,
            chosen: BTreeSet::new(),
            outcome: None,
            drafts: HashMap::new(),
            writes: HashMap::new(),
            unseen: HashMap::new(),
            _field_observation: observation,
        }
    }

    /// The write in flight in `project`, if any.
    pub(super) fn writing(&self, project: Option<&str>) -> Option<Operation> {
        Some(self.writes.get(project?)?.operation)
    }

    /// Put the current project's draft away and bring the next one's back.
    pub(super) fn switch_project(
        &mut self,
        from: Option<&str>,
        to: Option<&str>,
        cx: &mut Context<Inspector>,
    ) {
        let draft = self.field.read(cx).value().to_string();
        if let Some(from) = from {
            if draft.is_empty() {
                self.drafts.remove(from);
            } else {
                self.drafts.insert(from.to_string(), draft);
            }
        }
        let next = to
            .and_then(|to| self.drafts.get(to).cloned())
            .unwrap_or_default();
        self.field.update(cx, |field, cx| field.set_value(next, cx));
        self.visible = false;
        self.choice = CommitChoice::All;
        self.chosen.clear();
        // A write still running in either project keeps running, and is
        // reported in its own project when it ends.
        self.outcome = to.and_then(|to| self.unseen.remove(to));
    }
}

/// The scope a choice commits, given the repository as last read.
pub(super) fn commit_scope(
    choice: CommitChoice,
    chosen: &BTreeSet<String>,
    repository: &GitRepositoryInfo,
) -> CommitScope {
    match choice {
        CommitChoice::All => CommitScope::All,
        CommitChoice::Staged => CommitScope::Staged,
        CommitChoice::Chosen => {
            let files: Vec<_> = repository
                .files
                .iter()
                .filter(|file| chosen.contains(&file.path))
                .collect();
            let chosen = files.iter().map(|file| file.path.clone()).collect();
            let sources = files
                .iter()
                .filter_map(|file| file.rename_source().map(str::to_string))
                .collect();
            CommitScope::Paths { chosen, sources }
        }
    }
}

impl Inspector {
    /// The write in flight in the project shown.
    pub(super) fn running(&self) -> Option<Operation> {
        self.composer.writing(self.project.as_deref())
    }

    pub(super) fn toggle_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.visible = !self.composer.visible;
        self.composer.outcome = None;
        if self.composer.visible {
            // Reviewing the staged diff means committing exactly that.
            let staged = self
                .repository(cx)
                .is_some_and(|repo| repo.staged_file_count > 0);
            self.composer.choice = if staged && self.diff.mode == ComparisonMode::Staged {
                CommitChoice::Staged
            } else {
                CommitChoice::All
            };
            self.composer.chosen = self.diff.selected_path.iter().cloned().collect();
            self.composer.field.read(cx).focus_handle().focus(window);
        }
        cx.notify();
    }

    pub(super) fn commit(&mut self, message: String, cx: &mut Context<Self>) {
        let Some((root, repository, project)) = self.repository_target(cx) else {
            return;
        };
        if self.composer.writing(Some(&project)).is_some() {
            return;
        }
        let scope = commit_scope(self.composer.choice, &self.composer.chosen, &repository);
        let plan = match CommitPlan::new(message.trim(), &scope) {
            Ok(plan) => plan,
            Err(error) => {
                self.composer.outcome = Some(Outcome::Failed(Operation::Commit, error));
                cx.notify();
                return;
            }
        };
        self.start(Operation::Commit, project, cx, move || async move {
            fx_core::git::operations::commit(&root, &plan).await
        });
    }

    pub(super) fn push(&mut self, cx: &mut Context<Self>) {
        let Some((root, _, project)) = self.repository_target(cx) else {
            return;
        };
        if self.composer.writing(Some(&project)).is_some() {
            return;
        }
        self.start(Operation::Push, project, cx, move || async move {
            fx_core::git::operations::push(&root).await
        });
    }

    /// The repository to write to, and the project it belongs to.
    fn repository_target(
        &self,
        cx: &Context<Self>,
    ) -> Option<(std::path::PathBuf, GitRepositoryInfo, String)> {
        let monitor = self.monitor.read(cx);
        let snapshot = monitor.snapshot()?;
        Some((
            snapshot.root.clone(),
            snapshot.repository.clone()?,
            monitor.project()?.to_string(),
        ))
    }

    fn start<F, Fut>(
        &mut self,
        operation: Operation,
        project: String,
        cx: &mut Context<Self>,
        job: F,
    ) where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
    {
        self.composer.outcome = None;
        let job = repository::spawn_git(job);
        let target = project.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = repository::finish_git(job).await;
            let _ = this.update(cx, |this, cx| {
                this.finish_operation(operation, &target, result, cx)
            });
        });
        self.composer.writes.insert(
            project,
            Write {
                operation,
                _task: task,
            },
        );
        cx.notify();
    }

    /// Record how a write to `project` ended, whichever project is shown.
    ///
    /// Hidden, the inspector never hears of a switch from A to B and back, so
    /// a check of the monitor's generation here kept "Committing…" up for
    /// good. A commit's message is spent either way: its draft is removed, and
    /// the field cleared only while the field is that project's.
    pub(super) fn finish_operation(
        &mut self,
        operation: Operation,
        project: &str,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.composer.writes.remove(project);
        let shown = self.project.as_deref() == Some(project);
        let outcome = match result {
            Ok(summary) => {
                if operation == Operation::Commit {
                    self.composer.drafts.remove(project);
                    if shown {
                        self.composer
                            .field
                            .update(cx, |field, cx| field.set_value("", cx));
                        self.composer.visible = false;
                        self.composer.chosen.clear();
                    }
                }
                Outcome::Succeeded(summary)
            }
            Err(error) => Outcome::Failed(operation, error),
        };
        if shown {
            self.composer.outcome = Some(outcome);
        } else {
            self.composer.unseen.insert(project.to_string(), outcome);
        }
        if self.monitor.read(cx).project() == Some(project) {
            // Either way the tree may have changed; read it now, not in 5 s.
            self.monitor.update(cx, |monitor, cx| monitor.refresh(cx));
        }
        cx.notify();
    }

    /// Progress, or how the last commit or push ended.
    pub(super) fn operation_status(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let theme = global::current();
        if let Some(operation) = self.running() {
            return Some(
                div()
                    .debug_selector(|| "git-operation-progress".into())
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px_len(Spacing::XS))
                    .px(px_len(Spacing::MD))
                    .pb(px_len(Spacing::SM))
                    .child(Icon::ARROW_CLOCKWISE.render(
                        IconSize::Small,
                        Colors::fg_tertiary(&theme),
                        &theme,
                    ))
                    .child(
                        div()
                            .fx_caption()
                            .text_color(Colors::fg_secondary(&theme))
                            .child(operation.progress()),
                    )
                    .into_any_element(),
            );
        }
        let dismiss = icon_button(
            IconButtonProps::new(Icon::XMARK, "Dismiss").tiny().ghost(),
            cx.listener(|this, _, _, cx| {
                this.composer.outcome = None;
                cx.notify();
            }),
        );
        match self.composer.outcome.as_ref()? {
            Outcome::Succeeded(summary) => Some(
                div()
                    .debug_selector(|| "git-operation-success".into())
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px_len(Spacing::XS))
                    .pl(px_len(Spacing::MD))
                    .pr(px_len(Spacing::XS))
                    .child(Icon::CHECKMARK_CIRCLE_FILL.render(
                        IconSize::Small,
                        Colors::success(&theme),
                        &theme,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .fx_caption()
                            .text_color(Colors::fg_secondary(&theme))
                            .child(SharedString::from(summary.clone())),
                    )
                    .child(dismiss)
                    .into_any_element(),
            ),
            Outcome::Failed(operation, text) => {
                let copied = text.clone();
                let body = div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px_len(Spacing::XXS))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px_len(Spacing::XXS))
                            .child(
                                div()
                                    .flex_1()
                                    .fx_role(TextRole::CaptionMedium)
                                    .text_color(Colors::fg(&theme))
                                    .child(operation.failure()),
                            )
                            .child(icon_button(
                                IconButtonProps::new(Icon::DOC_ON_DOC, "Copy Git output")
                                    .tiny()
                                    .ghost(),
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()))
                                },
                            ))
                            .child(dismiss),
                    )
                    .child(
                        div()
                            .id("git-operation-output")
                            .debug_selector(|| "git-operation-error".into())
                            .max_h(px(Layout::DIFF_NAVIGATOR_COMPACT_HEIGHT))
                            .overflow_y_scroll()
                            .fx_role(TextRole::MonoSmall)
                            .text_color(Colors::fg_secondary(&theme))
                            .child(SharedString::from(text.clone())),
                    );
                Some(
                    div()
                        .flex_shrink_0()
                        .px(px_len(Spacing::MD))
                        .pb(px_len(Spacing::SM))
                        .child(fx_design::components::surface::banner(
                            Colors::error_muted(&theme),
                            Colors::fg(&theme),
                            body,
                        ))
                        .into_any_element(),
                )
            }
        }
    }

    pub(super) fn composer_view(&self, cx: &Context<Self>) -> AnyElement {
        let theme = global::current();
        let Some(repository) = self.repository(cx) else {
            return div().into_any_element();
        };
        let running = self.running().is_some();
        let message_empty = self.composer.field.read(cx).value().trim().is_empty();
        let choice = self.composer.choice;
        let chosen = self.composer.chosen.len();
        let can_commit = !running
            && !message_empty
            && (choice != CommitChoice::Chosen || chosen > 0)
            && (choice != CommitChoice::Staged || repository.staged_file_count > 0);
        let mut options = vec![SegmentedOption::new(CommitChoice::All, "All changes")];
        if repository.staged_file_count > 0 {
            options.push(SegmentedOption::new(CommitChoice::Staged, "Staged only"));
        }
        options.push(SegmentedOption::new(CommitChoice::Chosen, "Choose files"));
        let total = repository.files.len();
        let hint = match choice {
            CommitChoice::All => format!(
                "Stages all {total} changed {}, new files included.",
                plural(total, "file", "files")
            ),
            CommitChoice::Staged => format!(
                "Commits the {} staged {} and nothing else.",
                repository.staged_file_count,
                plural(repository.staged_file_count, "file", "files")
            ),
            CommitChoice::Chosen => format!(
                "Commits {chosen} of {total} {}; the rest stays as it is.",
                plural(total, "file", "files")
            ),
        };
        let mut view = div()
            .debug_selector(|| "commit-composer".into())
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::SM))
            .px(px_len(Spacing::MD))
            .py(px_len(Spacing::SM))
            .child(div().w_full().child(self.composer.field.clone()))
            .child(
                div()
                    .debug_selector(|| "commit-scope-picker".into())
                    .flex()
                    .child(segmented_control(
                        &options,
                        &choice,
                        cx.listener(|this, choice: &CommitChoice, _, cx| {
                            this.composer.choice = *choice;
                            cx.notify();
                        }),
                    )),
            );
        if choice == CommitChoice::Chosen {
            let mut list = div()
                .id("commit-file-list")
                .flex()
                .flex_col()
                .gap(px_len(Spacing::XXS))
                .max_h(px(Layout::DIFF_NAVIGATOR_COMPACT_HEIGHT))
                .overflow_y_scroll();
            for file in &repository.files {
                let path = file.path.clone();
                let checked = self.composer.chosen.contains(&path);
                list = list.child(checkbox(
                    SharedString::from(format!("commit-file:{path}")),
                    SharedString::from(format!("{}  {}", file.status(), path)),
                    checked,
                    !running,
                    cx.listener(move |this, _, _, cx| {
                        if !this.composer.chosen.remove(&path) {
                            this.composer.chosen.insert(path.clone());
                        }
                        cx.notify();
                    }),
                ));
            }
            let every: Vec<String> = repository.files.iter().map(|f| f.path.clone()).collect();
            view = view.child(list).child(
                div()
                    .flex()
                    .gap(px_len(Spacing::XS))
                    .child(compact_action_button(
                        "Select all",
                        None,
                        cx.listener(move |this, _, _, cx| {
                            this.composer.chosen = every.iter().cloned().collect();
                            cx.notify();
                        }),
                    ))
                    .child(compact_action_button(
                        "Select none",
                        None,
                        cx.listener(|this, _, _, cx| {
                            this.composer.chosen.clear();
                            cx.notify();
                        }),
                    )),
            );
        }
        view.child(
            div()
                .flex()
                .items_center()
                .gap(px_len(Spacing::SM))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .fx_caption()
                        .text_color(Colors::fg_tertiary(&theme))
                        .child(SharedString::from(hint)),
                )
                .child(
                    div()
                        .debug_selector(|| "commit-button".into())
                        .flex_shrink_0()
                        .child(button_with_icon(
                            if self.running() == Some(Operation::Commit) {
                                "Committing…"
                            } else {
                                "Commit"
                            },
                            Some(Icon::CHECKMARK),
                            ButtonStyle::Primary,
                            can_commit,
                            cx.listener(|this, _, _, cx| {
                                let message = this.composer.field.read(cx).value().to_string();
                                this.commit(message, cx);
                            }),
                        )),
                ),
        )
        .into_any_element()
    }
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::GitFileStatus;

    #[test]
    fn choosing_files_stages_only_what_needs_it_and_commits_a_renames_source() {
        let mut renamed = GitFileStatus::new("new.rs", "R", " ");
        renamed.original_path = Some("old.rs".into());
        // `status.renames=copies` reports a copy with its source, which is
        // still there and must not be committed as removed.
        let mut copied = GitFileStatus::new("copy.rs", "C", " ");
        copied.original_path = Some("source.rs".into());
        let repository = GitRepositoryInfo {
            is_git_repo: true,
            files: vec![
                renamed,
                copied,
                GitFileStatus::new("edited.rs", " ", "M"),
                GitFileStatus::new("fresh.rs", "?", "?"),
                GitFileStatus::new("left-alone.rs", "M", " "),
                GitFileStatus::new("source.rs", "M", " "),
            ],
            ..Default::default()
        };
        let chosen: BTreeSet<String> = ["new.rs", "copy.rs", "edited.rs", "fresh.rs"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            commit_scope(CommitChoice::Chosen, &chosen, &repository),
            CommitScope::Paths {
                chosen: vec![
                    "new.rs".into(),
                    "copy.rs".into(),
                    "edited.rs".into(),
                    "fresh.rs".into()
                ],
                sources: vec!["old.rs".into()],
            }
        );
        assert_eq!(
            commit_scope(CommitChoice::All, &chosen, &repository),
            CommitScope::All
        );
    }
}

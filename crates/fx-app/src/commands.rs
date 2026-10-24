//! The menu bar and the key bindings.
//!
//! Ported from `FlowXCommands.swift`. SwiftUI declared each command once, with
//! its menu, its shortcut, and its enabled state together; GPUI splits those
//! three apart, and this module is where they are put back in one place.
//!
//! # One table
//!
//! [`key_bindings`] is the only list of keys. [`install_key_bindings`]
//! registers it, the menu bar reads its shortcuts back out of the keymap it
//! made, and the Settings sheet's Shortcuts page is drawn from it. A binding
//! added here therefore appears in all three, and none of them can list a key
//! that does nothing.
//!
//! # Enabled state
//!
//! A menu item is enabled when its action has a handler in the focused
//! element's dispatch path — GPUI asks before the menu opens. So the root view
//! registers a handler only while its command can run (Stop only during a turn,
//! Reset only outside one), and the Edit items are enabled exactly when a text
//! field has focus, because the field is what handles them.
//!
//! # Rebuilding
//!
//! GPUI's menu bar is static, but the Threads menu names the active project's
//! threads. The root view keeps the [`MenuContext`] it last built from and
//! rebuilds only when that changes, rather than on every frame: AppKit
//! replaces the whole menu bar on every rebuild.

use gpui::{Action, App, KeyBinding, Keystroke, Menu, MenuItem, OsAction, SystemMenuType};

use crate::state::AppState;
use crate::text_field;

gpui::actions!(
    flowx,
    [
        /// Open the command palette, or close it when it is open.
        ToggleCommandPalette,
        /// Clear the active thread's conversation and start a fresh session.
        ResetConversation,
        /// Rename the active thread.
        RenameThread,
        /// Archive the active thread.
        ArchiveThread,
        /// Delete the active thread.
        DeleteThread,
        /// Open Settings on the archived chats.
        ShowArchivedChats,
        /// The placeholder row of an empty Threads menu. Nothing handles it,
        /// which is what keeps the row disabled.
        NoThreads,
        Minimize,
        Zoom,
        CloseWindow,
        Hide,
        HideOthers,
        ShowAll,
    ]
);

/// Open the active project's thread at `index`, in sidebar order: ⌘1 to ⌘9.
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = flowx, no_json)]
pub struct OpenThread {
    pub index: usize,
}

/// How many threads have a number key.
pub const NUMBERED_THREADS: usize = 9;

/// Where a binding is listed on the Shortcuts page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutGroup {
    General,
    Editing,
    View,
    Conversation,
    Threads,
    Window,
}

impl ShortcutGroup {
    pub const ALL: [Self; 6] = [
        Self::General,
        Self::Editing,
        Self::View,
        Self::Conversation,
        Self::Threads,
        Self::Window,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::View => "View",
            Self::Conversation => "Conversation",
            Self::Threads => "Threads",
            Self::Window => "Window",
        }
    }
}

/// One row of the binding table.
pub struct Shortcut {
    pub group: ShortcutGroup,
    /// GPUI's spelling, such as `cmd-shift-z`.
    pub keys: &'static str,
    /// What the Shortcuts page calls it.
    pub label: String,
    binding: KeyBinding,
}

impl Shortcut {
    fn new(
        group: ShortcutGroup,
        keys: &'static str,
        label: impl Into<String>,
        action: impl Action,
    ) -> Self {
        Self {
            group,
            keys,
            label: label.into(),
            binding: KeyBinding::new(keys, action, None),
        }
    }

    /// A binding that only applies inside one key context, such as the
    /// composer's.
    fn in_context(
        group: ShortcutGroup,
        keys: &'static str,
        label: impl Into<String>,
        action: impl Action,
        context: &str,
    ) -> Self {
        Self {
            group,
            keys,
            label: label.into(),
            binding: KeyBinding::new(keys, action, Some(context)),
        }
    }

    pub fn action(&self) -> &dyn Action {
        self.binding.action()
    }

    /// The keys as a Mac menu writes them: `⇧⌘Z`.
    pub fn symbols(&self) -> String {
        key_symbols(self.keys)
    }
}

/// Every app-wide key binding. One line per binding; see the module comment.
pub fn key_bindings() -> Vec<Shortcut> {
    use ShortcutGroup::*;
    let mut table = vec![
        Shortcut::new(General, "cmd-k", "Command palette", ToggleCommandPalette),
        Shortcut::new(General, "cmd-o", "Open project", crate::NewProject),
        Shortcut::new(General, "cmd-,", "Settings", crate::ShowSettings),
        Shortcut::new(General, "cmd-h", "Hide FlowX", Hide),
        Shortcut::new(General, "cmd-alt-h", "Hide others", HideOthers),
        Shortcut::new(General, "cmd-q", "Quit FlowX", crate::Quit),
        Shortcut::new(Editing, "cmd-z", "Undo", text_field::Undo),
        Shortcut::new(Editing, "cmd-shift-z", "Redo", text_field::Redo),
        Shortcut::new(Editing, "cmd-x", "Cut", text_field::Cut),
        Shortcut::new(Editing, "cmd-c", "Copy", text_field::Copy),
        Shortcut::new(Editing, "cmd-v", "Paste", text_field::Paste),
        Shortcut::new(Editing, "cmd-a", "Select all", text_field::SelectAll),
        Shortcut::new(View, "cmd-b", "Toggle sidebar", crate::ToggleSidebar),
        Shortcut::new(View, "cmd-g", "Toggle changes", crate::ToggleInspector),
        Shortcut::new(Conversation, "cmd-.", "Stop current run", crate::StopTurn),
        Shortcut::in_context(
            Conversation,
            "cmd-enter",
            "Send, or steer while responding",
            crate::composer::SubmitPrompt,
            crate::composer::KEY_CONTEXT,
        ),
        Shortcut::in_context(
            Conversation,
            "ctrl-enter",
            "Queue while responding",
            crate::composer::SubmitAlternate,
            crate::composer::KEY_CONTEXT,
        ),
        Shortcut::new(
            Conversation,
            "cmd-shift-a",
            "Attach images",
            crate::composer::AttachImages,
        ),
        Shortcut::new(Threads, "cmd-n", "New thread", crate::NewThread),
        Shortcut::new(Window, "cmd-m", "Minimize", Minimize),
        Shortcut::new(Window, "cmd-w", "Close window", CloseWindow),
    ];
    const NUMBER_KEYS: [&str; NUMBERED_THREADS] = [
        "cmd-1", "cmd-2", "cmd-3", "cmd-4", "cmd-5", "cmd-6", "cmd-7", "cmd-8", "cmd-9",
    ];
    for (index, keys) in NUMBER_KEYS.into_iter().enumerate() {
        table.push(Shortcut::new(
            Threads,
            keys,
            format!("Open thread {}", index + 1),
            OpenThread { index },
        ));
    }
    table
}

/// Register the whole table with the app's keymap.
pub fn install_key_bindings(cx: &mut App) {
    cx.bind_keys(key_bindings().into_iter().map(|shortcut| shortcut.binding));
}

/// The symbols for the binding that runs `action`, if the table has one.
pub fn shortcut_for(action: &dyn Action) -> Option<String> {
    key_bindings()
        .iter()
        .find(|shortcut| shortcut.action().partial_eq(action))
        .map(Shortcut::symbols)
}

/// GPUI's key spelling as a Mac menu writes it: `cmd-shift-z` is `⇧⌘Z`.
///
/// Modifiers come in Apple's order — Control, Option, Shift, Command — however
/// the binding spells them, so the same chord always reads the same way.
pub fn key_symbols(keys: &str) -> String {
    keys.split_whitespace()
        .map(|stroke| match Keystroke::parse(stroke) {
            Ok(keystroke) => {
                let modifiers = keystroke.modifiers;
                let mut label = String::new();
                for (held, symbol) in [
                    (modifiers.control, "⌃"),
                    (modifiers.alt, "⌥"),
                    (modifiers.shift, "⇧"),
                    (modifiers.platform, "⌘"),
                    (modifiers.function, "fn "),
                ] {
                    if held {
                        label.push_str(symbol);
                    }
                }
                label.push_str(&key_symbol(&keystroke.key));
                label
            }
            Err(_) => stroke.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn key_symbol(key: &str) -> String {
    match key {
        "enter" => "↩".into(),
        "backspace" => "⌫".into(),
        "delete" => "⌦".into(),
        "escape" => "⎋".into(),
        "tab" => "⇥".into(),
        "space" => "Space".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        other => other.to_uppercase(),
    }
}

// ── The menu bar ───────────────────────────────────────────────────────────

/// Everything the menu bar depends on that can change while the app runs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MenuContext {
    /// The active project's first nine threads' titles, in sidebar order.
    pub threads: Vec<String>,
    /// Whether any project has an archived thread to restore.
    pub has_archived: bool,
}

impl MenuContext {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            threads: state
                .active_agents()
                .into_iter()
                .take(NUMBERED_THREADS)
                .map(|agent| {
                    if agent.title.is_empty() {
                        "Untitled thread".to_string()
                    } else {
                        agent.title
                    }
                })
                .collect(),
            has_archived: state
                .snapshot
                .projects
                .iter()
                .any(|record| !record.archived_agent_ids.is_empty()),
        }
    }
}

/// The menu bar for `context`.
///
/// Shortcuts are not written here: AppKit shows whichever binding the keymap
/// holds for each item's action, which is the table above.
pub fn menus(context: &MenuContext) -> Vec<Menu> {
    let mut threads: Vec<MenuItem> = if context.threads.is_empty() {
        vec![MenuItem::action("No threads", NoThreads)]
    } else {
        context
            .threads
            .iter()
            .enumerate()
            .map(|(index, title)| MenuItem::action(format!("Open {title}"), OpenThread { index }))
            .collect()
    };
    threads.extend([
        MenuItem::separator(),
        MenuItem::action("Rename thread…", RenameThread),
        MenuItem::action("Archive thread", ArchiveThread),
        MenuItem::action("Delete thread…", DeleteThread),
    ]);
    if context.has_archived {
        threads.extend([
            MenuItem::separator(),
            MenuItem::action("Archived chats…", ShowArchivedChats),
        ]);
    }

    vec![
        Menu {
            name: "FlowX".into(),
            items: vec![
                MenuItem::action("Settings…", crate::ShowSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide FlowX", Hide),
                MenuItem::action("Hide others", HideOthers),
                MenuItem::action("Show all", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit FlowX", crate::Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open project…", crate::NewProject),
                MenuItem::action("New thread", crate::NewThread),
                MenuItem::separator(),
                MenuItem::action(
                    "Show project in Finder",
                    crate::sidebar::ShowProjectInFinder,
                ),
                MenuItem::action("Copy project path", crate::sidebar::CopyProjectPath),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", text_field::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", text_field::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", text_field::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", text_field::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", text_field::Paste, OsAction::Paste),
                MenuItem::os_action("Select all", text_field::SelectAll, OsAction::SelectAll),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command palette…", ToggleCommandPalette),
                MenuItem::separator(),
                MenuItem::action("Toggle sidebar", crate::ToggleSidebar),
                MenuItem::action("Toggle changes", crate::ToggleInspector),
            ],
        },
        Menu {
            name: "Conversation".into(),
            items: vec![
                MenuItem::action("Stop current run", crate::StopTurn),
                MenuItem::action("Attach images…", crate::composer::AttachImages),
                MenuItem::separator(),
                MenuItem::action("Reset conversation", ResetConversation),
            ],
        },
        Menu {
            name: "Threads".into(),
            items: threads,
        },
        // Named exactly "Window": GPUI hands a menu of that name to AppKit as
        // the windows menu, which is what lists open windows under it.
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::separator(),
                MenuItem::action("Close window", CloseWindow),
            ],
        },
    ]
}

/// Handlers for the commands that act on the application rather than a window.
pub fn install_app_handlers(cx: &mut App) {
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_binds_each_key_once_and_reads_as_mac_symbols() {
        let table = key_bindings();
        let mut keys: Vec<_> = table.iter().map(|shortcut| shortcut.keys).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), table.len(), "a key bound twice shadows one");
        assert_eq!(key_symbols("cmd-shift-z"), "⇧⌘Z");
        assert_eq!(key_symbols("shift-cmd-z"), "⇧⌘Z", "Apple's order, not ours");
        assert_eq!(key_symbols("cmd-alt-h"), "⌥⌘H");
        assert_eq!(key_symbols("ctrl-enter"), "⌃↩");
        assert_eq!(key_symbols("cmd-,"), "⌘,");
        assert_eq!(key_symbols("cmd-k cmd-s"), "⌘K ⌘S");
    }

    #[test]
    fn every_row_of_the_table_is_what_the_keymap_holds() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            install_key_bindings(cx);
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            for shortcut in key_bindings() {
                let bound: Vec<String> = keymap
                    .bindings_for_action(shortcut.action())
                    .map(|binding| {
                        binding
                            .keystrokes()
                            .iter()
                            .map(|stroke| stroke.inner().unparse())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect();
                let expected = Keystroke::parse(shortcut.keys).unwrap().unparse();
                assert_eq!(bound, vec![expected], "{}", shortcut.label);
            }
        });
    }

    #[test]
    fn the_number_keys_open_the_first_nine_threads_in_order() {
        let table = key_bindings();
        for index in 0..NUMBERED_THREADS {
            let shortcut = table
                .iter()
                .find(|shortcut| shortcut.action().partial_eq(&OpenThread { index }))
                .unwrap();
            assert_eq!(shortcut.symbols(), format!("⌘{}", index + 1));
        }
        assert_eq!(shortcut_for(&crate::NewThread).as_deref(), Some("⌘N"));
        assert_eq!(shortcut_for(&ResetConversation), None);
    }

    fn titles(menu: &Menu) -> Vec<String> {
        menu.items
            .iter()
            .map(|item| match item {
                MenuItem::Separator => "-".to_string(),
                MenuItem::Action { name, .. } => name.to_string(),
                MenuItem::Submenu(menu) => menu.name.to_string(),
                MenuItem::SystemMenu(menu) => menu.name.to_string(),
            })
            .collect()
    }

    fn menu<'a>(menus: &'a [Menu], name: &str) -> &'a Menu {
        menus.iter().find(|menu| menu.name == name).unwrap()
    }

    #[test]
    fn the_menu_bar_has_the_standard_menus_in_order() {
        let menus = menus(&MenuContext::default());
        let names: Vec<_> = menus.iter().map(|menu| menu.name.to_string()).collect();
        assert_eq!(
            names,
            [
                "FlowX",
                "File",
                "Edit",
                "View",
                "Conversation",
                "Threads",
                "Window"
            ]
        );
        assert_eq!(
            titles(menu(&menus, "Edit")),
            ["Undo", "Redo", "-", "Cut", "Copy", "Paste", "Select all"]
        );
        assert_eq!(
            titles(menu(&menus, "Window")),
            ["Minimize", "Zoom", "-", "Close window"]
        );
        assert!(titles(menu(&menus, "FlowX")).contains(&"Hide FlowX".to_string()));
        assert!(titles(menu(&menus, "Conversation")).contains(&"Reset conversation".to_string()));
        // The Edit items carry their system roles, so AppKit treats them as
        // the standard ones.
        assert!(menu(&menus, "Edit").items.iter().all(|item| matches!(
            item,
            MenuItem::Separator
                | MenuItem::Action {
                    os_action: Some(_),
                    ..
                }
        )));
    }

    #[test]
    fn the_threads_menu_names_the_active_threads_and_archives_only_when_there_are_some() {
        let empty = menus(&MenuContext::default());
        let threads = titles(menu(&empty, "Threads"));
        assert_eq!(threads[0], "No threads");
        assert!(!threads.contains(&"Archived chats…".to_string()));

        let context = MenuContext {
            threads: vec!["First".into(), "Second".into()],
            has_archived: true,
        };
        let full = menus(&context);
        let threads = menu(&full, "Threads");
        assert_eq!(
            titles(threads),
            [
                "Open First",
                "Open Second",
                "-",
                "Rename thread…",
                "Archive thread",
                "Delete thread…",
                "-",
                "Archived chats…"
            ]
        );
        let MenuItem::Action { action, .. } = &threads.items[1] else {
            panic!("an action row");
        };
        assert!(action.partial_eq(&OpenThread { index: 1 }));
    }
}

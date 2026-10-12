//! Menus: dropdowns and context menus.
//!
//! The Swift original deliberately avoided `NSMenu` and built flat panels on an
//! `NSPanel`, which meant every open panel also owned a separate window, a pair
//! of event monitors, and a `NSHashTable` hit-test tree for nested context
//! menus. None of that exists in GPUI, and none of it is needed: an
//! [`anchored`](gpui::anchored) element positions a panel against its anchor
//! and keeps it inside the window, so the whole popup layer is one absolutely
//! positioned element in the same window as its content.
//!
//! What that costs, and what this module therefore owns, is the *decision*
//! half — which item is highlighted, what the keyboard does, when a menu
//! closes. All of that is pure logic, so it lives in [`MenuModel`] and is
//! tested without a window.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How alarming a menu item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MenuItemTone {
    #[default]
    Standard,
    Destructive,
}

/// One row in a menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuItem {
    /// Stable within a menu. Used for the highlight key and for keyboard
    /// navigation, so it must not change between renders.
    pub id: String,
    pub title: String,
    /// Secondary line. Menus in this app are wide enough to show one, which is
    /// how a dropdown explains why an item is disabled or what it will do.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Whether this item is the current choice. Drawn with a checkmark.
    #[serde(default)]
    pub selected: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tone: MenuItemTone,
}

fn default_true() -> bool {
    true
}

impl MenuItem {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            selected: false,
            enabled: true,
            tone: MenuItemTone::default(),
        }
    }

    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn tone(mut self, tone: MenuItemTone) -> Self {
        self.tone = tone;
        self
    }
}

/// A labelled group of items, rendered under an uppercase header.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MenuSection {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub items: Vec<MenuItem>,
}

impl MenuSection {
    pub fn new(id: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            id: id.into(),
            title: None,
            items,
        }
    }

    pub fn titled(id: impl Into<String>, title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            id: id.into(),
            title: Some(title.into()),
            items,
        }
    }
}

/// A whole menu: sections plus the highlight state that navigation mutates.
///
/// This is the testable half of the popup layer. Rendering reads it; keyboard
/// and pointer input mutate it.
#[derive(Debug, Clone, Default)]
pub struct MenuModel {
    sections: Vec<MenuSection>,
    /// Highlight key, formatted `section|item` so it survives a rebuild of the
    /// sections with different indices.
    highlighted: Option<String>,
    /// Items that have run their action, for tests that need to observe a
    /// choice without standing up a window.
    last_activated: Option<String>,
}

impl MenuModel {
    pub fn new(sections: Vec<MenuSection>) -> Self {
        Self {
            sections,
            highlighted: None,
            last_activated: None,
        }
    }

    pub fn sections(&self) -> &[MenuSection] {
        &self.sections
    }

    /// The highlighted item's key.
    pub fn highlighted(&self) -> Option<&str> {
        self.highlighted.as_deref()
    }

    /// The key of the item that ran its action, if any.
    pub fn last_activated(&self) -> Option<&str> {
        self.last_activated.as_deref()
    }

    /// Build the highlight key for one item.
    ///
    /// Public because the render layer uses the same key for row element ids,
    /// and a row's id must be stable across frames for hover to work.
    pub fn key_for(section_id: &str, item_id: &str) -> String {
        format!("{section_id}|{item_id}")
    }

    /// Every enabled item, in display order, with its key.
    ///
    /// Disabled rows are excluded entirely rather than skipped during
    /// navigation, so arrow keys move between things the user can actually
    /// choose.
    fn navigable(&self) -> Vec<(String, MenuItem)> {
        let mut out = Vec::new();
        for section in &self.sections {
            for item in &section.items {
                if item.enabled {
                    out.push((Self::key_for(&section.id, &item.id), item.clone()));
                }
            }
        }
        out
    }

    /// Whether any item can be chosen at all.
    pub fn has_navigable_items(&self) -> bool {
        self.navigable()
            .iter()
            .any(|(k, _)| Some(k.as_str()) != self.highlighted.as_deref())
    }

    /// Move the highlight by `delta`, wrapping at both ends.
    ///
    /// Returns whether the highlight actually moved, which is `false` when
    /// nothing is navigable.
    pub fn move_highlight(&mut self, delta: isize) -> bool {
        let items = self.navigable();
        if items.is_empty() {
            self.highlighted = None;
            return false;
        }

        let current = self
            .highlighted
            .as_deref()
            .and_then(|key| items.iter().position(|(k, _)| k == key));

        let next = match current {
            // Nothing highlighted yet: the first move lands on an end, so
            // pressing Down opens onto the first item and Up onto the last.
            None => {
                if delta < 0 {
                    items.len() - 1
                } else {
                    0
                }
            }
            Some(index) => {
                let len = items.len() as isize;
                (((index as isize + delta) % len + len) % len) as usize
            }
        };

        self.highlighted = Some(items[next].0.clone());
        true
    }

    /// Highlight the first navigable item.
    pub fn highlight_first(&mut self) -> bool {
        self.highlighted = None;
        self.move_highlight(1)
    }

    /// Highlight the last navigable item.
    pub fn highlight_last(&mut self) -> bool {
        self.highlighted = None;
        self.move_highlight(-1)
    }

    /// Highlight a specific item, if it is navigable.
    pub fn highlight(&mut self, section_id: &str, item_id: &str) -> bool {
        let key = Self::key_for(section_id, item_id);
        let exists = self.navigable().iter().any(|(k, _)| k == &key);
        if exists {
            self.highlighted = Some(key);
        }
        // A key that vanished with a rebuild must not linger.
        self.reconcile_highlight();
        exists
    }

    /// Choose the highlighted item.
    ///
    /// Returns the chosen item's id, or `None` when nothing is highlighted or
    /// the highlighted row is disabled.
    pub fn activate(&mut self) -> Option<String> {
        let key = self.highlighted.clone()?;
        for section in &self.sections {
            for item in &section.items {
                if Self::key_for(&section.id, &item.id) == key {
                    if !item.enabled {
                        return None;
                    }
                    self.last_activated = Some(item.id.clone());
                    return Some(item.id.clone());
                }
            }
        }
        None
    }

    /// Set the highlight to a full key, if it names an existing item.
    ///
    /// Used by the pointer, which knows the whole key, where
    /// [`highlight`](Self::highlight) takes the section and item separately.
    pub fn set_highlight(&mut self, key: &str) -> bool {
        let exists = self
            .sections
            .iter()
            .flat_map(|s| s.items.iter())
            .any(|item| {
                self.sections
                    .iter()
                    .any(|section| Self::key_for(&section.id, &item.id) == key)
            });
        if exists {
            self.highlighted = Some(key.to_string());
        } else {
            self.highlighted = None;
        }
        exists
    }

    /// Drop a highlight that no longer names an existing item.
    ///
    /// Called after a rebuild, because a menu is often re-created from fresh
    /// state each time it opens — a stale key would silently freeze keyboard
    /// navigation.
    pub fn reconcile_highlight(&mut self) {
        if let Some(key) = self.highlighted.clone() {
            let still_there = self
                .sections
                .iter()
                .flat_map(|s| s.items.iter())
                .any(|item| {
                    self.sections
                        .iter()
                        .any(|section| Self::key_for(&section.id, &item.id) == key)
                });
            if !still_there {
                self.highlighted = None;
            }
        }
    }

    /// Every item key mapped to its display title, for accessibility and tests.
    pub fn item_titles(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for section in &self.sections {
            for item in &section.items {
                out.insert(Self::key_for(&section.id, &item.id), item.title.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> MenuModel {
        MenuModel::new(vec![
            MenuSection::titled(
                "provider",
                "Provider",
                vec![
                    MenuItem::new("codex", "Codex").selected(true),
                    MenuItem::new("claude", "Claude"),
                ],
            ),
            MenuSection::titled(
                "models",
                "Models",
                vec![
                    MenuItem::new("astra", "GPT-6 Astra").subtitle("272K context"),
                    MenuItem::new("sol", "GPT-5.6 Sol").subtitle("272K context"),
                    MenuItem::new("opus", "Claude Opus 4.8")
                        .subtitle("1M context")
                        .enabled(false),
                ],
            ),
        ])
    }

    fn extended_model() -> MenuModel {
        MenuModel::new(vec![MenuSection::new(
            "s",
            vec![
                MenuItem::new("a", "A"),
                MenuItem::new("b", "B"),
                MenuItem::new("c", "C"),
                MenuItem::new("d", "D"),
            ],
        )])
    }

    fn disabled_item() -> MenuItem {
        MenuItem::new("x", "X").enabled(false)
    }

    #[test]
    fn a_new_menu_has_no_highlight() {
        assert_eq!(model().highlighted(), None);
    }

    #[test]
    fn moving_down_from_nothing_lands_on_the_first_item() {
        let mut m = model();
        assert!(m.move_highlight(1));
        assert_eq!(m.highlighted(), Some("provider|codex"));
    }

    #[test]
    fn moving_up_from_nothing_lands_on_the_last_item() {
        let mut m = model();
        assert!(m.move_highlight(-1));
        // The last *enabled* item; the disabled Opus row is skipped.
        assert_eq!(m.highlighted(), Some("models|sol"));
    }

    #[test]
    fn navigation_skips_disabled_items() {
        let mut m = MenuModel::new(vec![MenuSection::new(
            "s",
            vec![
                MenuItem::new("a", "A"),
                disabled_item(),
                MenuItem::new("c", "C"),
            ],
        )]);
        m.move_highlight(1);
        assert_eq!(m.highlighted(), Some("s|a"));
        m.move_highlight(1);
        assert_eq!(m.highlighted(), Some("s|c"), "the disabled row was skipped");
        m.move_highlight(1);
        assert_eq!(m.highlighted(), Some("s|a"), "wrapped past the end");
    }

    #[test]
    fn navigation_wraps_at_both_ends() {
        let mut m = extended_model();
        m.highlight_first();
        assert_eq!(m.highlighted(), Some("s|a"));
        m.move_highlight(-1);
        assert_eq!(m.highlighted(), Some("s|d"), "wrapped backwards to the end");
        m.move_highlight(1);
        assert_eq!(
            m.highlighted(),
            Some("s|a"),
            "wrapped forwards to the start"
        );
    }

    #[test]
    fn a_large_step_lands_correctly_modulo_the_length() {
        // Four enabled items, a..d, so index = position in that list.
        let mut m = extended_model();
        m.highlight_first();
        m.move_highlight(3);
        assert_eq!(m.highlighted(), Some("s|d"), "index 0 + 3 = 3");
        // 3 - 4 = -1, which wraps to 3, not 0. Wrapping is modular, not clamped.
        m.move_highlight(-4);
        assert_eq!(m.highlighted(), Some("s|d"), "-1 wrapped to 3");
        // 3 - 3 = 0.
        m.move_highlight(-3);
        assert_eq!(m.highlighted(), Some("s|a"));
    }

    #[test]
    fn repeated_moves_return_to_the_start() {
        let mut m = extended_model();
        m.highlight_first();
        for _ in 0..4 {
            m.move_highlight(1);
        }
        assert_eq!(m.highlighted(), Some("s|a"), "a full cycle lands back on a");
    }

    #[test]
    fn a_full_key_can_be_highlighted_directly() {
        let mut m = model();
        assert!(m.set_highlight("models|astra"));
        assert_eq!(m.highlighted(), Some("models|astra"));
        assert!(!m.set_highlight("models|nope"));
        assert_eq!(m.highlighted(), None, "a bad key clears the highlight");
    }

    #[test]
    fn highlighting_an_explicit_item_wins_over_navigation() {
        let mut m = model();
        assert!(m.highlight("models", "astra"));
        assert_eq!(m.highlighted(), Some("models|astra"));
    }

    #[test]
    fn highlighting_a_disabled_item_is_refused() {
        let mut m = model();
        assert!(!m.highlight("models", "opus"));
    }

    #[test]
    fn activating_returns_the_chosen_item() {
        let mut m = model();
        m.highlight("models", "sol");
        assert_eq!(m.activate(), Some("sol".into()));
        assert_eq!(m.last_activated(), Some("sol"));
    }

    #[test]
    fn activating_without_a_highlight_does_nothing() {
        let mut m = model();
        assert_eq!(m.activate(), None);
        assert_eq!(m.last_activated(), None);
    }

    #[test]
    fn a_menu_of_only_disabled_items_cannot_be_navigated() {
        let mut m = MenuModel::new(vec![MenuSection::new(
            "s",
            vec![disabled_item(), disabled_item()],
        )]);
        assert!(!m.move_highlight(1));
        assert_eq!(m.highlighted(), None);
        assert_eq!(m.activate(), None);
    }

    #[test]
    fn an_empty_menu_is_inert() {
        let mut m = MenuModel::default();
        assert!(!m.move_highlight(1));
        assert!(!m.move_highlight(-1));
        assert!(!m.highlight_first());
        assert!(!m.highlight_last());
        assert_eq!(m.activate(), None);
    }

    #[test]
    fn a_fresh_model_starts_with_no_highlight() {
        // Menus are re-created from fresh state each time one opens, so a new
        // model must never inherit a highlight from a previous one.
        let rebuilt = MenuModel::new(vec![MenuSection::new(
            "models",
            vec![MenuItem::new("opus", "Claude Opus 4.8")],
        )]);
        assert_eq!(rebuilt.highlighted(), None);
    }

    #[test]
    fn reconciliation_keeps_a_highlight_that_still_exists() {
        let mut m = model();
        m.highlight("models", "sol");
        m.reconcile_highlight();
        assert_eq!(
            m.highlighted(),
            Some("models|sol"),
            "reconciliation must not clear a valid highlight"
        );
    }

    #[test]
    fn a_disabled_row_cannot_receive_the_highlight() {
        // Opus is present but disabled, so it is not a navigation target. A
        // failed `highlight` must leave the previous state alone rather than
        // silently clearing it.
        let mut m = model();
        m.highlight("models", "sol");
        assert!(!m.highlight("models", "opus"));
        assert_eq!(m.highlighted(), Some("models|sol"));
    }

    #[test]
    fn reconciliation_clears_a_key_whose_item_vanished() {
        let mut m = model();
        m.highlight("models", "sol");
        // Simulate the item disappearing without rebuilding the model.
        m.sections[1].items.retain(|i| i.id != "sol");
        m.reconcile_highlight();
        assert_eq!(m.highlighted(), None, "a stale key would freeze navigation");
    }

    #[test]
    fn item_keys_are_scoped_by_section() {
        // Two sections may both contain an item called "default"; the keys must
        // not collide or the highlight would jump between them.
        let mut m = MenuModel::new(vec![
            MenuSection::new("a", vec![MenuItem::new("same", "A")]),
            MenuSection::new("b", vec![MenuItem::new("same", "B")]),
        ]);
        m.highlight("a", "same");
        assert_eq!(m.highlighted(), Some("a|same"));
        m.highlight("b", "same");
        assert_eq!(m.highlighted(), Some("b|same"));
    }

    #[test]
    fn selected_state_is_carried_on_the_item() {
        let m = model();
        let codex = m.sections()[0].items[0].clone();
        assert!(codex.selected);
        assert_eq!(codex.id, "codex");
        assert!(!m.sections()[0].items[1].selected);
    }

    #[test]
    fn subtitles_are_preserved_for_display() {
        let m = model();
        assert_eq!(
            m.sections()[1].items[0].subtitle.as_deref(),
            Some("272K context")
        );
        assert_eq!(m.sections()[0].items[0].subtitle, None);
    }

    #[test]
    fn item_titles_index_every_item_by_key() {
        let m = model();
        let titles = m.item_titles();
        assert_eq!(
            titles.get("provider|codex").map(String::as_str),
            Some("Codex")
        );
        assert_eq!(
            titles.get("models|sol").map(String::as_str),
            Some("GPT-5.6 Sol")
        );
        assert_eq!(titles.len(), 5);
    }

    #[test]
    fn a_model_round_trips_through_json() {
        let m = MenuModel::new(vec![MenuSection::titled(
            "s",
            "Section",
            vec![
                MenuItem::new("a", "A").selected(true),
                MenuItem::new("b", "B").tone(MenuItemTone::Destructive),
                MenuItem::new("c", "C").enabled(false),
            ],
        )]);
        let json = serde_json::to_string(&m.sections).unwrap();
        let back: Vec<MenuSection> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m.sections);
    }
}

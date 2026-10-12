//! Access to the active theme from anywhere in a render pass.
//!
//! GPUI has no equivalent of Swift's `FXTheme` global statics, but it also does
//! not need one: GPUI renders on a single thread, so a thread-local is both
//! sufficient and cheap, and it keeps the "views read `Colors::fg(&theme)`,
//! never a literal" contract intact without threading a theme handle through
//! every component signature.
//!
//! The trade-off is deliberate: a globally-read theme means a component cannot
//! be rendered against two themes at once. That is fine here because the app has
//! exactly one window per theme.

use std::cell::RefCell;

use super::color::Theme;

thread_local! {
    static CURRENT: RefCell<Theme> = RefCell::new(Theme::default());
}

/// The theme in effect on this thread.
pub fn current() -> Theme {
    CURRENT.with(|t| *t.borrow())
}

/// Replace the active theme.
///
/// Call this from the preferences store whenever appearance, tone, accent, or
/// text size changes, then notify the root view so every dependent redraws.
pub fn set(theme: Theme) {
    CURRENT.with(|t| *t.borrow_mut() = theme);
}

/// Mutate the active theme in place.
pub fn update(f: impl FnOnce(&mut Theme)) {
    CURRENT.with(|cell| f(&mut cell.borrow_mut()));
}

/// Set the resolved system appearance, which only matters when
/// `appearance_mode` is `System`. Returns the theme so a caller can pass it
/// straight into a notification.
pub fn set_system_appearance(is_dark: bool) -> Theme {
    update(|t| t.system_appearance = is_dark);
    current()
}

/// A guard that restores the previous theme when dropped.
///
/// Useful in tests, where each case wants a known theme without leaking its
/// choice into the next.
pub struct ThemeGuard {
    previous: Theme,
}

impl ThemeGuard {
    /// Install `theme` for the duration of the guard.
    pub fn install(theme: Theme) -> Self {
        let previous = current();
        set(theme);
        Self { previous }
    }
}

impl Drop for ThemeGuard {
    fn drop(&mut self) {
        set(self.previous);
    }
}

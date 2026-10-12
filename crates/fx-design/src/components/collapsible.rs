//! A measured disclosure with an interruptible height transition.

use std::{cell::Cell, rc::Rc, time::Instant};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, LayoutId,
    Pixels, SharedString, Window, div, px,
};

use crate::theme::animation::PanelTransition;

/// Reveal or conceal content at its natural height. The content remains mounted
/// while clipped, so callers must disable actions when `expanded` is false.
/// Keeping its measured height also avoids jumps when reversing a transition.
pub fn collapsible(
    id: impl Into<SharedString>,
    expanded: bool,
    content: impl IntoElement,
) -> impl IntoElement {
    Collapsible {
        id: id.into(),
        expanded,
        content: Some(content.into_any_element()),
    }
}

struct Collapsible {
    id: SharedString,
    expanded: bool,
    content: Option<AnyElement>,
}

struct DisclosureState {
    transition: PanelTransition,
    natural_height: Rc<Cell<Option<Pixels>>>,
}

impl IntoElement for Collapsible {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Collapsible {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone().into())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        window.with_element_state(
            global_id.unwrap(),
            |state: Option<DisclosureState>, window| {
                let now = Instant::now();
                let mut state = state.unwrap_or_else(|| DisclosureState {
                    transition: PanelTransition::new(self.expanded),
                    natural_height: Rc::new(Cell::new(None)),
                });
                state.transition.set_expanded(self.expanded, now);
                let progress = state.transition.progress(now);
                if state.transition.is_animating(now) {
                    // This is an element layout pass, not a view's render function.
                    window.request_animation_frame();
                }

                let height = state.natural_height.clone();
                let debug_label = format!("collapsible:{}", self.id);
                let mut wrapper = div()
                    .debug_selector(move || debug_label.clone())
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .flex_shrink_0()
                            .child(self.content.take().expect("content is laid out once")),
                    )
                    .on_children_prepainted(move |bounds, window, _| {
                        if let Some(bounds) = bounds.first()
                            && height.replace(Some(bounds.size.height)) != Some(bounds.size.height)
                            && progress < 1.0
                        {
                            window.request_animation_frame();
                        }
                    });
                // At rest an expanded section sizes naturally, including when sync
                // adds threads or a status line wraps after a sidebar resize.
                if progress < 1.0 {
                    wrapper = wrapper.h(state.natural_height.get().unwrap_or(px(0.0)) * progress);
                }
                let mut wrapper = wrapper.into_any_element();
                ((wrapper.request_layout(window, cx), wrapper), state)
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        content: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        content.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        content: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        content.paint(window, cx);
    }
}

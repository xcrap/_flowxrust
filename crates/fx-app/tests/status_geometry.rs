//! Native layout regressions for layered design-system controls.

use fx_design::components::{ActivityState, activity_dot, context_ring_icon};
use fx_design::icon::Icon;
use fx_design::theme::{Colors, IconSize, Typography, global};
use gpui::prelude::*;
use gpui::{Context, Render, Window, div, px};

struct StatusProbe;

impl Render for StatusProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        div()
            .flex()
            .gap(px(20.0))
            .child(context_ring_icon(
                Icon::ARROW_UP,
                Some(0.4),
                Colors::accent(&theme),
                36.0,
                true,
                false,
                |_, _, _| {},
            ))
            .child(activity_dot(
                Colors::accent(&theme),
                ActivityState::Idle,
                0.0,
            ))
    }
}

#[test]
fn send_ring_layers_keep_their_dimensions_and_share_one_center() {
    let mut test = gpui::TestAppContext::single();
    let window = test.add_window(|_, _| StatusProbe);
    test.refresh().unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
    let outer = visual.debug_bounds("context-ring").unwrap();
    let track = visual.debug_bounds("context-ring-track").unwrap();
    let fill = visual.debug_bounds("context-ring-fill").unwrap();
    let glyph = visual.debug_bounds("context-ring-glyph").unwrap();
    assert_eq!(outer.size, gpui::size(px(36.0), px(36.0)));
    assert_eq!(track, outer);
    assert_eq!(fill.size, gpui::size(px(28.0), px(28.0)));
    let edge = px(Typography::icon(IconSize::Control, &global::current()).size);
    assert_eq!(glyph.size, gpui::size(edge, edge));
    assert_eq!(fill.center(), outer.center());
    assert_eq!(glyph.center(), outer.center());
}

#[test]
fn activity_ring_and_identity_dot_overlay_instead_of_becoming_neighbors() {
    let mut test = gpui::TestAppContext::single();
    let window = test.add_window(|_, _| StatusProbe);
    test.refresh().unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
    let outer = visual.debug_bounds("activity-dot").unwrap();
    let ring = visual.debug_bounds("activity-ring").unwrap();
    let dot = visual.debug_bounds("activity-inner").unwrap();
    assert_eq!(outer.size, gpui::size(px(16.0), px(16.0)));
    assert_eq!(ring.size, gpui::size(px(13.0), px(13.0)));
    assert_eq!(dot.size, gpui::size(px(7.0), px(7.0)));
    assert_eq!(ring.center(), outer.center());
    assert_eq!(dot.center(), outer.center());
}

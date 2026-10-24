//! Native layout regressions for layered design-system controls.

use fx_design::components::{
    ActivityState, ButtonStyle, ContextUsage, MenuItem, MenuModel, MenuSection, ProviderIdentity,
    activity_dot, button_with_icon, context_meter, context_ring_icon, menu_surface,
    provider_status,
};
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

struct RecheckProbe;

impl Render for RecheckProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().flex().items_start().child(button_with_icon(
            "Re-check",
            Some(Icon::ARROW_CLOCKWISE),
            ButtonStyle::Secondary,
            true,
            |_, _, _| {},
        ))
    }
}

#[test]
fn labelled_controls_keep_icon_and_label_in_one_compact_row() {
    let mut test = gpui::TestAppContext::single();
    let window = test.add_window(|_, _| RecheckProbe);
    test.refresh().unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
    let button = visual.debug_bounds("button-content").unwrap();
    let icon = visual.debug_bounds("button-icon").unwrap();
    let label = visual.debug_bounds("button-label").unwrap();
    assert!(
        button.size.height >= px(28.0) && button.size.height <= px(32.0),
        "{button:?}"
    );
    assert_eq!(icon.center().y, label.center().y);
    assert!(icon.right() < label.left());
    assert!(label.right() < button.right());
}

struct MenuProbe;

impl Render for MenuProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        menu_surface(
            &MenuModel::new(vec![MenuSection::new(
                "choices",
                vec![MenuItem::new("one", "Supervised").subtitle("Ask before making changes")],
            )]),
            |_, _, _, _| {},
            |_, _, _| {},
        )
    }
}

#[test]
fn menu_rows_grow_to_fit_their_supporting_label() {
    let mut test = gpui::TestAppContext::single();
    let window = test.add_window(|_, _| MenuProbe);
    test.refresh().unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
    let row = visual.debug_bounds("menu-row").unwrap();
    let title = visual.debug_bounds("menu-title").unwrap();
    let subtitle = visual.debug_bounds("menu-subtitle").unwrap();
    assert!(title.bottom() < subtitle.top());
    assert!(subtitle.bottom() < row.bottom());
    assert!(row.size.height > px(28.0));
}

struct ProviderProbe(ActivityState);

impl Render for ProviderProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .child(provider_status(
                "test-status",
                ProviderIdentity::Claude,
                self.0,
                "Status",
            ))
            .child(context_meter(
                ContextUsage {
                    used: Some(50_000),
                    limit: Some(200_000),
                },
                ProviderIdentity::Claude,
            ))
    }
}

#[test]
fn live_provider_states_paint_without_moving_or_growing_the_context_control() {
    let mut test = gpui::TestAppContext::single();
    let window = test.add_window(|_, _| ProviderProbe(ActivityState::Idle));
    let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
    let mut initial = None;
    for activity in ActivityState::ALL {
        window
            .update(&mut test, |probe, _, cx| {
                probe.0 = activity;
                cx.notify();
            })
            .unwrap();
        test.refresh().unwrap();
        let tile = visual.debug_bounds("provider-status").unwrap();
        let meter = visual.debug_bounds("composer-context").unwrap();
        assert_eq!(tile.size, gpui::size(px(20.0), px(20.0)));
        assert!(tile.right() <= meter.left());
        assert!(meter.size.width < px(65.0));
        if let Some(previous) = initial {
            assert_eq!((tile, meter), previous);
        }
        initial = Some((tile, meter));
    }
}

struct CountingProbe {
    activity: ActivityState,
    renders: std::rc::Rc<std::cell::Cell<usize>>,
}

impl Render for CountingProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        div().child(provider_status(
            "counted",
            ProviderIdentity::Codex,
            self.activity,
            "Working",
        ))
    }
}

#[test]
fn a_moving_status_tile_repaints_on_a_bounded_clock_and_a_settled_one_stops() {
    use fx_design::theme::FxAnimation;
    use std::time::Duration;

    let mut test = gpui::TestAppContext::single();
    let renders = std::rc::Rc::new(std::cell::Cell::new(0));
    let window = test.add_window({
        let renders = renders.clone();
        move |_, _| CountingProbe {
            activity: ActivityState::Running,
            renders,
        }
    });
    test.refresh().unwrap();
    test.run_until_parked();

    // One repaint per tick keeps the spinner moving.
    let start = renders.get();
    for _ in 0..10 {
        test.executor().advance_clock(FxAnimation::ACTIVITY_FRAME);
        test.run_until_parked();
    }
    assert_eq!(renders.get() - start, 10, "one repaint per frame interval");

    // Time shorter than the interval asks for nothing more.
    let before = renders.get();
    test.executor()
        .advance_clock(FxAnimation::ACTIVITY_FRAME / 2 - Duration::from_millis(1));
    test.run_until_parked();
    assert_eq!(renders.get(), before, "no repaint before the interval");

    // Settled: the pending tick lands once, then nothing.
    for activity in [ActivityState::Waiting, ActivityState::Idle] {
        window
            .update(&mut test, |probe, _, cx| {
                probe.activity = activity;
                cx.notify();
            })
            .unwrap();
        test.run_until_parked();
    }
    test.executor().advance_clock(FxAnimation::ACTIVITY_FRAME);
    test.run_until_parked();
    let settled = renders.get();
    for _ in 0..10 {
        test.executor().advance_clock(FxAnimation::ACTIVITY_FRAME);
        test.run_until_parked();
    }
    assert_eq!(renders.get(), settled, "a settled tile stops repainting");
}

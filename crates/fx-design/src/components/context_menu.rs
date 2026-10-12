//! Context menus: right-click regions and keyboard-driven open menus.
//!
//! The Swift original built a tree of hit-testable regions in the environment
//! and resolved a context click by walking children first, so the *innermost*
//! non-empty region under the pointer won — which is how nested context menus
//! behave. That tree is invisible to GPUI, so this module rebuilds it from what
//! the render pass actually knows.
//!
//! ## How it works
//!
//! During prepaint, an element that carries a context menu records the clipped
//! rectangle GPUI gives it. Those rectangles accumulate in a [`ContextRegistry`] owned by
//! the app. A right-click anywhere in the window asks the registry which region
//! contains the cursor; the deepest match wins, because a nested region's rect
//! is contained by its parent's.
//!
//! Registration is per frame. That is deliberate: a region only exists while
//! the thing that owns it is on screen, so a scrolled-away row stops offering a
//! context menu without any explicit bookkeeping.

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, Entity, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, Pixels, SharedString, Window,
};

use crate::components::menu::MenuLayer;
use crate::components::menu_model::{MenuModel, MenuSection};

/// One context-menu-capable rectangle.
#[derive(Debug, Clone)]
pub struct ContextRegion {
    /// The rect GPUI assigned during layout, in window coordinates.
    pub bounds: gpui::Bounds<gpui::Pixels>,
    /// Nesting depth. Larger is more deeply nested, and wins a tie.
    pub depth: usize,
    /// Owner identity, so two regions can be told apart.
    pub owner: gpui::SharedString,
    /// What to show when this region is clicked.
    pub sections: Vec<MenuSection>,
    /// `None` means this region accepts the click but shows nothing, which is
    /// how the original avoided a spurious menu over a row that has no items.
    pub menu: Option<MenuModel>,
    /// Registration order within the frame, used to break ties at equal depth.
    /// Assigned by [`ContextRegistry::register`], not by callers.
    order: usize,
}

impl ContextRegion {
    /// Comparability is by rectangle and ownership, not by the menu contents:
    /// two regions are "the same" when they cover the same place for the same
    /// owner, which is what a test about precedence actually cares about.
    pub fn same_place_as(&self, other: &ContextRegion) -> bool {
        self.bounds == other.bounds && self.owner == other.owner && self.depth == other.depth
    }

    /// Whether the region contains a point.
    ///
    /// Scroll clipping matters: a row taller than its viewport still reports
    /// its full bounds unless the parent has already clipped it, so the
    /// resolved rect is what should be tested, not the laid-out one.
    pub fn contains(&self, point: gpui::Point<gpui::Pixels>) -> bool {
        point.x >= self.bounds.origin.x
            && point.x <= self.bounds.origin.x + self.bounds.size.width
            && point.y >= self.bounds.origin.y
            && point.y <= self.bounds.origin.y + self.bounds.size.height
    }

    /// How specific this region is.
    ///
    /// A nested region wins over its ancestor; between two equally nested
    /// regions the later one wins, because it was registered closer to the
    /// pointer's final position.
    pub fn precedence(&self) -> (usize, usize) {
        (self.depth, self.order)
    }
}

/// Every context region registered by the most recent render pass.
#[derive(Default)]
pub struct ContextRegistry {
    regions: Vec<ContextRegion>,
    /// Monotonic order, used to break ties between equal depths.
    next_order: usize,
    /// The prepaint nesting of transparent context-region wrappers.
    active_depth: usize,
}

impl ContextRegistry {
    /// Forget the previous frame's regions.
    ///
    /// Called once per frame before child prepaint, so a region only lives as long as
    /// the element that registered it.
    pub fn reset(&mut self) {
        self.regions.clear();
        self.next_order = 0;
        self.active_depth = 0;
    }

    /// Record a region. Called during prepaint.
    pub fn register(&mut self, mut region: ContextRegion) {
        region.order = self.next_order;
        self.next_order += 1;
        self.regions.push(region);
    }

    pub fn regions(&self) -> &[ContextRegion] {
        &self.regions
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// The region a context click at `point` should open.
    ///
    /// The **deepest** region containing the point is authoritative, and if it
    /// has no menu then nothing opens.
    ///
    /// This is a deliberate departure from the original, which made an empty
    /// region transparent so its ancestor's menu would appear instead. That
    /// sounds convenient until a row that happens to have no applicable actions
    /// is clicked and the user gets menu items that belong to something else —
    /// an "Expand / Collapse" from a diff section, say, when the cursor was
    /// over a single code line. Opening nothing is recoverable; acting on the
    /// wrong row is not.
    ///
    /// Returns `None` when nothing under the pointer offers a menu, which is
    /// the common case and must stay silent.
    pub fn resolve(&self, point: gpui::Point<gpui::Pixels>) -> Option<&ContextRegion> {
        let deepest = self.region_at(point)?;
        deepest.menu.as_ref()?;
        Some(deepest)
    }

    /// Whether any registered region covers `point`, menu or not.
    ///
    /// Used to distinguish "right-clicked a row that has no menu" from
    /// "right-clicked the background", which the app may want to treat
    /// differently.
    pub fn region_at(&self, point: gpui::Point<gpui::Pixels>) -> Option<&ContextRegion> {
        self.regions
            .iter()
            .filter(|r| r.contains(point))
            .max_by_key(|r| r.precedence())
    }
}

/// Wrap the window's root once. Resetting before child prepaint ties the
/// registry to the displayed frame, including scrolls and hidden rows.
pub fn context_frame(registry: &Entity<ContextRegistry>, content: impl IntoElement) -> AnyElement {
    ContextTracker {
        registry: registry.clone(),
        content: content.into_any_element(),
        region: None,
        reset: true,
    }
    .into_any_element()
}

/// Attach a menu to an element's visible rectangle without changing its layout
/// or installing a competing per-row mouse handler. Nested wrappers receive
/// deeper precedence automatically; `None` deliberately blocks an ancestor.
pub fn context_region(
    registry: &Entity<ContextRegistry>,
    owner: impl Into<SharedString>,
    menu: Option<MenuModel>,
    content: impl IntoElement,
) -> AnyElement {
    ContextTracker {
        registry: registry.clone(),
        content: content.into_any_element(),
        region: Some((owner.into(), menu)),
        reset: false,
    }
    .into_any_element()
}

struct ContextTracker {
    registry: Entity<ContextRegistry>,
    content: AnyElement,
    region: Option<(SharedString, Option<MenuModel>)>,
    reset: bool,
}

impl IntoElement for ContextTracker {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for ContextTracker {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.content.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.reset {
            self.registry.update(cx, |registry, _| registry.reset());
        }
        let depth = self.region.as_ref().map(|(owner, menu)| {
            // GPUI's content mask already includes every enclosing scroll
            // viewport and the window. Layout bounds alone include hidden rows.
            let clipped = bounds.intersect(&window.content_mask().bounds);
            self.registry.update(cx, |registry, _| {
                let depth = registry.active_depth;
                registry.active_depth += 1;
                if !clipped.is_empty() {
                    registry.register(ContextRegion {
                        bounds: clipped,
                        depth,
                        owner: owner.clone(),
                        sections: menu
                            .as_ref()
                            .map(|menu| menu.sections().to_vec())
                            .unwrap_or_default(),
                        menu: menu.clone(),
                        order: 0,
                    });
                }
                depth
            })
        });
        self.content.prepaint(window, cx);
        if let Some(depth) = depth {
            self.registry
                .update(cx, |registry, _| registry.active_depth = depth);
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.paint(window, cx);
    }
}

/// Open a context menu at a point, if a region covers it.
///
/// Returns whether a menu opened, so a caller can fall through to some other
/// right-click behavior when nothing did.
pub fn open_at(
    registry: &Entity<ContextRegistry>,
    layer: &Entity<MenuLayer>,
    point: gpui::Point<gpui::Pixels>,
    cx: &mut App,
) -> bool {
    let opened = registry.read(cx).resolve(point).map(|region| {
        (
            region.owner.clone(),
            region
                .menu
                .clone()
                .expect("resolve only returns regions with a menu"),
        )
    });
    match opened {
        Some((owner, menu)) => {
            layer.update(cx, |layer, cx| {
                layer.open_at(menu, owner, point);
                cx.notify();
            });
            true
        }
        None => false,
    }
}

/// Handle a right-click anywhere in the window.
///
/// Attach this to the window's root element. It stays silent unless a region
/// offers a menu, so right-clicking the background does nothing rather than
/// opening an empty panel.
pub fn handle_root_right_click(
    registry: &Entity<ContextRegistry>,
    layer: &Entity<MenuLayer>,
    event: &MouseDownEvent,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if event.button != MouseButton::Right {
        return false;
    }
    let opened = open_at(registry, layer, event.position, cx);
    if opened {
        cx.stop_propagation();
        window.refresh();
    }
    opened
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> gpui::Bounds<gpui::Pixels> {
        gpui::Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn region(
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        depth: usize,
        owner: gpui::SharedString,
        with_menu: bool,
    ) -> ContextRegion {
        let menu = with_menu.then(|| {
            MenuModel::new(vec![MenuSection::new(
                "s",
                vec![crate::components::MenuItem::new("a", "A")],
            )])
        });
        ContextRegion {
            bounds: bounds(x, y, w, h),
            depth,
            owner,
            sections: Vec::new(),
            menu,
            order: 0,
        }
    }

    #[test]
    fn an_empty_registry_resolves_nothing() {
        let registry = ContextRegistry::default();
        assert!(registry.is_empty());
        assert!(registry.resolve(point(px(10.0), px(10.0))).is_none());
        assert!(registry.region_at(point(px(10.0), px(10.0))).is_none());
    }

    #[test]
    fn a_region_is_resolved_by_its_rectangle() {
        let mut registry = ContextRegistry::default();
        registry.register(region(
            0.0,
            0.0,
            100.0,
            50.0,
            0,
            gpui::SharedString::from("row"),
            true,
        ));

        assert!(registry.resolve(point(px(50.0), px(25.0))).is_some());
        assert!(registry.resolve(point(px(150.0), px(25.0))).is_none());
        assert!(registry.resolve(point(px(50.0), px(80.0))).is_none());
    }

    #[test]
    fn edges_are_inside_the_region() {
        let mut registry = ContextRegistry::default();
        registry.register(region(
            10.0,
            10.0,
            50.0,
            20.0,
            0,
            gpui::SharedString::from("row"),
            true,
        ));
        for p in [
            point(px(10.0), px(10.0)),
            point(px(60.0), px(30.0)),
            point(px(35.0), px(10.0)),
            point(px(10.0), px(30.0)),
        ] {
            assert!(
                registry.resolve(p).is_some(),
                "the corner {p:?} was excluded"
            );
        }
    }

    #[test]
    fn a_deeper_region_wins_over_its_ancestor() {
        // The nested case: a row inside a section inside a file group.
        let mut registry = ContextRegistry::default();
        registry.register(region(
            0.0,
            0.0,
            400.0,
            600.0,
            0,
            gpui::SharedString::from("window"),
            true,
        ));
        registry.register(region(
            0.0,
            100.0,
            400.0,
            400.0,
            1,
            gpui::SharedString::from("group"),
            true,
        ));
        registry.register(region(
            0.0,
            200.0,
            400.0,
            40.0,
            2,
            gpui::SharedString::from("row"),
            true,
        ));

        assert_eq!(
            registry.resolve(point(px(10.0), px(210.0))).unwrap().owner,
            gpui::SharedString::from("row")
        );
        assert_eq!(
            registry.resolve(point(px(10.0), px(150.0))).unwrap().owner,
            gpui::SharedString::from("group")
        );
        assert_eq!(
            registry.resolve(point(px(10.0), px(50.0))).unwrap().owner,
            gpui::SharedString::from("window")
        );
    }

    #[test]
    fn a_region_without_a_menu_blocks_its_ancestor() {
        // Deliberate divergence: an empty row opens nothing rather than
        // inheriting its group's items, which would act on the wrong target.
        let mut registry = ContextRegistry::default();
        registry.register(region(
            0.0,
            0.0,
            400.0,
            600.0,
            0,
            gpui::SharedString::from("window"),
            true,
        ));
        registry.register(region(
            0.0,
            100.0,
            400.0,
            40.0,
            1,
            gpui::SharedString::from("empty-row"),
            false,
        ));

        assert!(
            registry.resolve(point(px(10.0), px(110.0))).is_none(),
            "an empty row opened its ancestor's menu, which would act on the wrong target"
        );
        // Outside the empty row, the ancestor still answers.
        assert!(
            registry.resolve(point(px(10.0), px(50.0))).is_some(),
            "the ancestor menu stopped working entirely"
        );
        // It is still *covered*, which the app may want to distinguish.
        assert_eq!(
            registry
                .region_at(point(px(10.0), px(110.0)))
                .unwrap()
                .owner,
            "empty-row"
        );
    }

    #[test]
    fn equal_depths_break_ties_by_registration_order() {
        let mut registry = ContextRegistry::default();
        registry.register(region(
            0.0,
            0.0,
            400.0,
            600.0,
            0,
            gpui::SharedString::from("first"),
            true,
        ));
        registry.register(region(
            0.0,
            0.0,
            400.0,
            600.0,
            0,
            gpui::SharedString::from("second"),
            true,
        ));
        assert_eq!(
            registry.resolve(point(px(10.0), px(10.0))).unwrap().owner,
            gpui::SharedString::from("second"),
            "the most recently registered region won"
        );
    }

    #[test]
    fn resetting_clears_the_previous_frames_regions() {
        let mut registry = ContextRegistry::default();
        registry.register(region(
            0.0,
            0.0,
            100.0,
            100.0,
            0,
            gpui::SharedString::from("row"),
            true,
        ));
        assert_eq!(registry.len(), 1);

        registry.reset();
        assert!(registry.is_empty());
        assert!(
            registry.resolve(point(px(10.0), px(10.0))).is_none(),
            "a region outlived the element that registered it"
        );
    }

    #[test]
    fn regions_outside_the_window_are_not_resolved() {
        let mut registry = ContextRegistry::default();
        registry.register(region(
            -500.0,
            -500.0,
            100.0,
            100.0,
            0,
            gpui::SharedString::from("offscreen"),
            true,
        ));
        assert!(registry.resolve(point(px(10.0), px(10.0))).is_none());
        assert!(registry.resolve(point(px(-490.0), px(-490.0))).is_some());
    }
}

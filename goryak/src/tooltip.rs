use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use yakui_core::dom::Dom;
use yakui_core::event::{EventInterest, EventResponse, WidgetEvent};
use yakui_core::geometry::{Constraints, Dim2, Rect, Vec2};
use yakui_core::layout::LayoutDom;
use yakui_core::paint::{PaintDom, Pipeline};
use yakui_core::widget::{EventContext, LayoutContext, PaintContext, Widget};
use yakui_core::{context, Alignment, Flow, WidgetId};
use yakui_widgets::util::widget_children;

use crate::{on_primary, padxy, primary, round_rect, textc};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Label {
    text: Cow<'static, str>,
    delay: Duration,
}

#[derive(Debug)]
struct HitRegion {
    rect: Rect,
    layer: usize,
    label: Option<Label>,
    takes_pointer: bool,
}

#[derive(Debug)]
struct PaintedRegion {
    rect: Rect,
    points: [Vec2; 3],
    layer: usize,
}

impl PaintedRegion {
    fn contains(&self, point: Vec2) -> bool {
        if !self.rect.contains_point(point) {
            return false;
        }
        let [a, b, c] = self.points;
        let ab = (b - a).perp_dot(point - a);
        let bc = (c - b).perp_dot(point - b);
        let ca = (a - c).perp_dot(point - c);
        (ab >= 0.0 && bc >= 0.0 && ca >= 0.0) || (ab <= 0.0 && bc <= 0.0 && ca <= 0.0)
    }
}

#[derive(Debug)]
struct Hover {
    label: Label,
    since: Instant,
}

/// Where the next triangle of a paint layer goes: a call and an index into its indices.
type PaintCursor = (usize, usize);

/// Triangles painted by a [`pass_pointer`] subtree.
#[derive(Debug)]
struct PassSpan {
    layer: usize,
    triangles: Range<PaintCursor>,
    /// Layers the subtree opened, painted by it alone.
    layers: Range<usize>,
}

impl PassSpan {
    fn contains(&self, layer: usize, triangle: PaintCursor) -> bool {
        self.layers.contains(&layer) || (self.layer == layer && self.triangles.contains(&triangle))
    }
}

#[derive(Debug, Default)]
struct TooltipState {
    pointer: Option<Vec2>,
    hovered: Option<Hover>,
    regions: Vec<HitRegion>,
    painted: Vec<PaintedRegion>,
    passes: Vec<PassSpan>,
}

impl TooltipState {
    fn refresh_hover(&mut self) {
        let mut front_layer = 0;
        let mut label = None;
        if let Some(pointer) = self.pointer {
            for hit in &self.regions {
                if !hit.rect.contains_point(pointer) || hit.layer < front_layer {
                    continue;
                }
                if hit.layer > front_layer || hit.label.is_some() {
                    front_layer = hit.layer;
                    label = hit.label.as_ref();
                }
            }
            if label.is_some()
                && self
                    .painted
                    .iter()
                    .any(|hit| hit.layer > front_layer && hit.contains(pointer))
            {
                label = None;
            }
        }
        if label != self.hovered.as_ref().map(|hover| &hover.label) {
            self.hovered = label.cloned().map(|label| Hover {
                label,
                since: Instant::now(),
            });
        }
    }

    fn ready_label(&self) -> Option<&Label> {
        let hover = self.hovered.as_ref()?;
        (hover.since.elapsed() >= hover.label.delay).then_some(&hover.label)
    }
}

type SharedState = Rc<RefCell<TooltipState>>;

thread_local! {
    static LAST_PAINTED_UI: RefCell<Weak<RefCell<TooltipState>>> = const { RefCell::new(Weak::new()) };
}

/// Test the last rendered Yakui layout at a point in physical screen pixels.
pub fn pointer_over_ui(x: f32, y: f32) -> bool {
    let point = Vec2::new(x, y);
    LAST_PAINTED_UI.with_borrow(|state| {
        state.upgrade().is_some_and(|state| {
            let state = state.borrow();
            state
                .regions
                .iter()
                .any(|hit| hit.takes_pointer && hit.rect.contains_point(point))
                || state.painted.iter().any(|hit| hit.contains(point))
        })
    })
}

/// Draw content that floats over the world without capturing it, such as markers anchored
/// to world positions. Neither its paint nor its widgets count for [`pointer_over_ui`], so
/// world tools keep working under it. Its widgets still get their own events, and may sink
/// them as usual.
pub fn pass_pointer<T>(children: impl FnOnce() -> T) -> T {
    let dom = context::dom();
    let pass = dom.begin_widget::<PassPointer>(());
    let response = children();
    dom.end_widget::<PassPointer>(pass.id);
    response
}

#[derive(Debug)]
struct PassPointer;

impl PassPointer {
    fn cursor(paint: &PaintDom) -> Option<(usize, PaintCursor)> {
        let layers = paint.layers();
        let current = layers.current()?;
        let layer = layers
            .iter()
            .position(|layer| std::ptr::eq(layer, current))?;
        let call = current.calls.len().saturating_sub(1);
        let index = current.calls.last().map_or(0, |call| call.indices.len());
        Some((layer, (call, index)))
    }
}

impl Widget for PassPointer {
    type Props<'a> = ();
    type Response = ();

    fn new() -> Self {
        Self
    }

    fn update(&mut self, _: Self::Props<'_>) {}

    fn paint(&self, mut ctx: PaintContext<'_>) {
        let start = Self::cursor(ctx.paint);
        let first_layer = ctx.paint.layers().len();
        let node = ctx.dom.get_current();
        for &child in &node.children {
            ctx.paint(child);
        }
        let end = Self::cursor(ctx.paint);
        let span = match (start, end) {
            (Some((layer, start)), Some((_, end))) => PassSpan {
                layer,
                triangles: start..end,
                layers: first_layer..ctx.paint.layers().len(),
            },
            _ => return,
        };
        let state = ctx.dom.get_global_or_init(SharedState::default);
        state.borrow_mut().passes.push(span);
    }
}

/// Wrap a control with a hover label. Returns the control's own response.
pub fn tooltip<T>(text: impl Into<Cow<'static, str>>, children: impl FnOnce() -> T) -> T {
    tooltip_after(text.into(), Duration::ZERO, children)
}

pub(crate) fn tooltip_after<T>(
    text: Cow<'static, str>,
    delay: Duration,
    children: impl FnOnce() -> T,
) -> T {
    if text.is_empty() {
        return children();
    }
    let dom = context::dom();
    let hover = dom.begin_widget::<TooltipHover>(Label { text, delay });
    let response = children();
    dom.end_widget::<TooltipHover>(hover.id);
    response
}

/// Render a single hover label after the rest of the Yakui UI.
pub fn render_tooltip() {
    let state = context::dom().get_global_or_init(SharedState::default);
    let shown = state.borrow().ready_label().cloned();
    widget_children::<TooltipLayer, _>(
        || {
            if let Some(label) = &shown {
                round_rect(5.0, primary(), || {
                    padxy(6.0, 4.0, || {
                        textc(on_primary(), label.text.clone());
                    });
                });
            }
        },
        (state, shown.clone()),
    );
}

#[derive(Debug)]
struct TooltipHover(Label);

impl Widget for TooltipHover {
    type Props<'a> = Label;
    type Response = ();

    fn new() -> Self {
        Self(Label {
            text: Cow::Borrowed(""),
            delay: Duration::ZERO,
        })
    }

    fn update(&mut self, label: Label) {
        self.0 = label;
    }
}

#[derive(Debug, Default)]
struct TooltipLayer {
    state: SharedState,
    shown: Option<Label>,
}

impl Widget for TooltipLayer {
    type Props<'a> = (SharedState, Option<Label>);
    type Response = ();

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, (state, shown): Self::Props<'_>) {
        self.state = state;
        self.shown = shown;
    }

    fn flow(&self) -> Flow {
        Flow::Relative {
            anchor: Alignment::TOP_LEFT,
            offset: Dim2::ZERO,
        }
    }

    fn event_interest(&self) -> EventInterest {
        EventInterest::MOUSE_MOVE
    }

    fn event(&mut self, ctx: EventContext<'_>, event: &WidgetEvent) -> EventResponse {
        if let WidgetEvent::MouseMoved(pointer) = *event {
            let mut state = self.state.borrow_mut();
            let pointer = pointer.map(|point| {
                point * ctx.layout.scale_factor() + ctx.layout.unscaled_viewport().pos()
            });
            if pointer != state.pointer {
                state.pointer = pointer;
                state.hovered = None;
                state.refresh_hover();
            }
        }
        EventResponse::Bubble
    }

    fn layout(&self, mut ctx: LayoutContext<'_>, _: Constraints) -> Vec2 {
        ctx.layout.new_layer(ctx.dom);
        let viewport = ctx.layout.viewport().size();
        let origin = ctx.layout.unscaled_viewport().pos();
        let pointer = self.state.borrow().pointer.unwrap_or(origin);
        let pointer = (pointer - origin) / ctx.layout.scale_factor();
        for &child in &ctx.dom.get_current().children {
            let size = ctx.calculate_layout(child, Constraints::loose(viewport));
            let mut position = pointer + Vec2::new(12.0, 16.0);
            if position.y + size.y > viewport.y {
                position.y = pointer.y - size.y - 8.0;
            }
            let limit = (viewport - size).max(Vec2::ZERO);
            ctx.layout.set_pos(child, position.clamp(Vec2::ZERO, limit));
        }
        Vec2::ZERO
    }

    fn paint(&self, ctx: PaintContext<'_>) {
        let mut state = self.state.borrow_mut();
        state.regions.clear();
        collect_regions(
            ctx.dom,
            ctx.layout,
            ctx.dom.root(),
            &mut 0,
            0,
            false,
            &mut state.regions,
        );
        let passes = std::mem::take(&mut state.passes);
        collect_painted_regions(ctx.paint, &passes, &mut state.painted);
        LAST_PAINTED_UI.set(Rc::downgrade(&self.state));
        state.refresh_hover();

        // Layout is final here. Hide an obsolete bubble immediately when its control
        // moves or is covered. A newly exposed label can be built on the next frame.
        if self.shown.is_some() && self.shown.as_ref() == state.ready_label() {
            self.default_paint(ctx);
        }
    }
}

fn collect_painted_regions(
    paint: &PaintDom,
    passes: &[PassSpan],
    regions: &mut Vec<PaintedRegion>,
) {
    regions.clear();
    let size = paint.surface_size();
    let surface = Rect::from_pos_size(Vec2::ZERO, size);
    // Yakui batches faces from arbitrary widgets together. Use their triangles so
    // visible backgrounds count without filling clipped or unpainted gaps.
    // The tooltip itself has yet to paint, so it remains transparent to input.
    for (layer, painted_layer) in paint.layers().iter().enumerate() {
        for (call_index, call) in painted_layer.calls.iter().enumerate() {
            if call.pipeline != Pipeline::Main {
                continue;
            }
            let clip = call.clip.map_or(surface, |clip| surface.constrain(clip));
            for (triangle_index, triangle) in call.indices.chunks_exact(3).enumerate() {
                let cursor = (call_index, triangle_index * 3);
                if passes.iter().any(|pass| pass.contains(layer, cursor)) {
                    continue;
                }
                let [a, b, c] = [triangle[0], triangle[1], triangle[2]]
                    .map(|index| call.vertices[usize::from(index)]);
                if a.color.w <= 0.0 && b.color.w <= 0.0 && c.color.w <= 0.0 {
                    continue;
                }
                let [a, b, c] = [a, b, c].map(|vertex| vertex.position * size);
                if (b - a).perp_dot(c - a) == 0.0 {
                    continue;
                }
                let min = a.min(b).min(c);
                let max = a.max(b).max(c);
                let rect = Rect::from_pos_size(min, max - min).constrain(clip);
                if rect.size().min_element() > 0.0 {
                    regions.push(PaintedRegion {
                        rect,
                        points: [a, b, c],
                        layer,
                    });
                }
            }
        }
    }
}

fn collect_regions(
    dom: &Dom,
    layout: &LayoutDom,
    id: WidgetId,
    next_layer: &mut usize,
    mut layer: usize,
    mut passes: bool,
    regions: &mut Vec<HitRegion>,
) {
    let Some(layout_node) = layout.get(id) else {
        return;
    };
    let node = dom.get(id).unwrap();
    if node.widget.is::<TooltipLayer>() {
        return;
    }
    if layout_node.new_layer {
        layer = *next_layer;
        *next_layer += 1;
    }
    passes |= node.widget.is::<PassPointer>();
    let label = node.widget.downcast_ref::<TooltipHover>();
    let takes_pointer = !passes
        && layout_node
            .event_interest
            .contains(EventInterest::MOUSE_INSIDE);
    if (label.is_some() || takes_pointer) && layout_node.rect.size().min_element() > 0.0 {
        let mut rect = layout_node.rect;
        let mut clip = layout_node.clipped_by;
        while let Some(clip_id) = clip {
            let clip_node = layout.get(clip_id).unwrap();
            rect = rect.constrain(clip_node.rect);
            clip = clip_node.clipped_by;
        }
        let scale = layout.scale_factor();
        let rect = Rect::from_pos_size(
            rect.pos() * scale + layout.unscaled_viewport().pos(),
            rect.size() * scale,
        );
        if rect.size().min_element() > 0.0 {
            regions.push(HitRegion {
                rect,
                layer,
                label: label.map(|hover| hover.0.clone()),
                takes_pointer,
            });
        }
    }
    for &child in &node.children {
        collect_regions(dom, layout, child, next_layer, layer, passes, regions);
    }
}

#[cfg(test)]
fn tooltip_painted(ui: &mut yakui_core::Yakui) -> bool {
    ui.paint()
        .layers()
        .last()
        .is_some_and(|layer| !layer.calls.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yakui_core::event::Event;
    use yakui_core::geometry::Rect;
    use yakui_core::input::MouseButton;
    use yakui_core::{WidgetId, Yakui};
    use yakui_widgets::widgets::Button;

    fn frame(ui: &mut Yakui) -> (WidgetId, bool, usize) {
        ui.start();
        let button = tooltip("Hover label", || Button::styled("Open").show());
        render_tooltip();
        ui.finish();
        let overlay_count = usize::from(tooltip_painted(ui)) + 1;
        (button.id, button.clicked, overlay_count)
    }

    #[test]
    fn p4_tooltip_shows_over_a_button_and_preserves_clicks() {
        let mut ui = Yakui::new();
        let viewport = Vec2::new(400.0, 160.0);
        ui.set_surface_size(viewport);
        ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));

        let (id, _, children) = frame(&mut ui);
        assert_eq!(children, 1);
        let button = ui.layout_dom().get(id).unwrap().rect;
        let pointer = button.pos() + button.size() * 0.5;
        ui.handle_event(Event::CursorMoved(Some(pointer)));
        assert_eq!(frame(&mut ui).2, 2);

        ui.handle_event(Event::MouseButtonChanged {
            button: MouseButton::One,
            down: true,
        });
        ui.handle_event(Event::MouseButtonChanged {
            button: MouseButton::One,
            down: false,
        });
        assert!(frame(&mut ui).1);

        ui.handle_event(Event::CursorMoved(None));
        assert_eq!(frame(&mut ui).2, 1);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use yakui_core::event::Event;
    use yakui_core::geometry::Rect;
    use yakui_core::Yakui;
    use yakui_widgets::widgets::{Button, Layer};

    fn has_tooltip(ui: &mut Yakui, position: Vec2, covered: bool) -> bool {
        ui.start();
        yakui_widgets::offset(position, || {
            tooltip("Hover label", || Button::styled("Open").show());
        });
        if covered {
            Layer::new().show(|| {
                yakui_widgets::opaque(|| {
                    yakui_widgets::constrained(Constraints::tight(Vec2::splat(80.0)), || ());
                });
            });
        }
        render_tooltip();
        ui.finish();
        tooltip_painted(ui)
    }

    fn ui() -> Yakui {
        let mut ui = Yakui::new();
        let viewport = Vec2::new(400.0, 160.0);
        ui.set_surface_size(viewport);
        ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));
        ui
    }

    #[test]
    fn p4d_stationary_pointer_rechecks_layout() {
        let mut ui = ui();
        assert!(!has_tooltip(&mut ui, Vec2::ZERO, false));
        ui.handle_event(Event::CursorMoved(Some(Vec2::new(15.0, 12.0))));
        assert!(has_tooltip(&mut ui, Vec2::ZERO, false));
        assert!(!has_tooltip(&mut ui, Vec2::new(120.0, 0.0), false));
        assert!(!has_tooltip(&mut ui, Vec2::ZERO, false));
        assert!(has_tooltip(&mut ui, Vec2::ZERO, false));
    }

    #[test]
    fn p4d_higher_layer_occludes_a_tooltip() {
        let mut ui = ui();
        assert!(!has_tooltip(&mut ui, Vec2::ZERO, true));
        ui.handle_event(Event::CursorMoved(Some(Vec2::new(15.0, 12.0))));
        assert!(!has_tooltip(&mut ui, Vec2::ZERO, true));
    }

    #[test]
    fn p4d_painted_foreground_occludes_a_tooltip_without_capture() {
        let mut ui = ui();
        let frame = |ui: &mut Yakui, covered| {
            ui.start();
            tooltip("Hover label", || Button::styled("Open").show());
            if covered {
                Layer::new().show(|| {
                    crate::RoundRect::new(4.0)
                        .min_size(Vec2::splat(80.0))
                        .show();
                });
            }
            render_tooltip();
            ui.finish();
            tooltip_painted(ui)
        };
        assert!(!frame(&mut ui, false));
        ui.handle_event(Event::CursorMoved(Some(Vec2::new(15.0, 12.0))));
        assert!(frame(&mut ui, false));
        assert!(!frame(&mut ui, true));
    }
    #[test]
    fn finalnative_pass_pointer_content_leaves_the_world_reachable() {
        let mut ui = ui();
        ui.start();
        pass_pointer(|| {
            crate::RoundRect::new(4.0)
                .min_size(Vec2::splat(80.0))
                .show();
        });
        yakui_widgets::offset(Vec2::new(0.0, 100.0), || {
            pass_pointer(|| Button::styled("Marker").show());
        });
        // Ordinary UI painted afterwards over part of it still counts.
        yakui_widgets::offset(Vec2::new(40.0, 0.0), || {
            crate::RoundRect::new(4.0)
                .min_size(Vec2::splat(80.0))
                .show();
        });
        render_tooltip();
        ui.finish();
        ui.paint();
        assert!(!pointer_over_ui(20.0, 20.0));
        assert!(!pointer_over_ui(10.0, 110.0));
        assert!(pointer_over_ui(60.0, 20.0));
        assert!(pointer_over_ui(100.0, 20.0));
    }
}

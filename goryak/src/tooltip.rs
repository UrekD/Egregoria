use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use yakui_core::event::{EventInterest, EventResponse, WidgetEvent};
use yakui_core::geometry::{Constraints, Dim2, Vec2};
use yakui_core::widget::{EventContext, LayoutContext, Widget};
use yakui_core::{context, Alignment, Flow};
use yakui_widgets::util::widget_children;

use crate::{on_primary, padxy, primary, round_rect, textc};

struct TooltipRequest {
    text: Cow<'static, str>,
    pointer: Vec2,
}

type TooltipState = Rc<RefCell<Option<TooltipRequest>>>;

/// Wrap a control with a hover label. Returns the control's own response.
pub fn tooltip<T>(text: impl Into<Cow<'static, str>>, children: impl FnOnce() -> T) -> T {
    let dom = context::dom();
    let hover = dom.begin_widget::<TooltipHover>(());
    let response = children();
    dom.end_widget::<TooltipHover>(hover.id);

    if let Some(pointer) = *hover {
        let text = text.into();
        if !text.is_empty() {
            let state = dom.get_global_or_init(TooltipState::default);
            *state.borrow_mut() = Some(TooltipRequest { text, pointer });
        }
    }
    response
}

/// Paint the current hover label at the root of the Yakui frame.
pub fn render_tooltip() {
    let state = context::dom().get_global_or_init(TooltipState::default);
    let Some(TooltipRequest { text, pointer }) = state.borrow_mut().take() else {
        return;
    };
    widget_children::<TooltipBubble, _>(
        || {
            round_rect(5.0, primary(), || {
                padxy(6.0, 4.0, || {
                    textc(on_primary(), text);
                });
            });
        },
        pointer,
    );
}

#[derive(Debug, Default)]
struct TooltipHover {
    pointer: Option<Vec2>,
}

impl Widget for TooltipHover {
    type Props<'a> = ();
    type Response = Option<Vec2>;

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, _: Self::Props<'_>) -> Self::Response {
        self.pointer
    }

    fn event_interest(&self) -> EventInterest {
        EventInterest::MOUSE_MOVE
    }

    fn event(&mut self, ctx: EventContext<'_>, event: &WidgetEvent) -> EventResponse {
        if let WidgetEvent::MouseMoved(pointer) = *event {
            // Buttons consume mouse-enter events before the enclosing widget receives them.
            self.pointer = pointer.filter(|&pointer| {
                let mut id = Some(ctx.dom.current());
                while let Some(widget) = id {
                    let Some(node) = ctx.layout.get(widget) else {
                        return false;
                    };
                    if !node.rect.contains_point(pointer) {
                        return false;
                    }
                    id = node.clipped_by;
                }
                true
            });
        }
        EventResponse::Bubble
    }
}

#[derive(Debug, Default)]
struct TooltipBubble {
    pointer: Vec2,
}

impl Widget for TooltipBubble {
    type Props<'a> = Vec2;
    type Response = ();

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, pointer: Vec2) {
        self.pointer = pointer;
    }

    fn flow(&self) -> Flow {
        Flow::Relative {
            anchor: Alignment::TOP_LEFT,
            offset: Dim2::ZERO,
        }
    }

    fn layout(&self, mut ctx: LayoutContext<'_>, _: Constraints) -> Vec2 {
        ctx.layout.new_layer(ctx.dom);
        let viewport = ctx.layout.viewport().size();
        for &child in &ctx.dom.get_current().children {
            let size = ctx.calculate_layout(child, Constraints::loose(viewport));
            let mut position = self.pointer + Vec2::new(12.0, 16.0);
            if position.y + size.y > viewport.y {
                position.y = self.pointer.y - size.y - 8.0;
            }
            let limit = (viewport - size).max(Vec2::ZERO);
            ctx.layout.set_pos(child, position.clamp(Vec2::ZERO, limit));
        }
        Vec2::ZERO
    }
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
        let overlay_count = ui.dom().get(ui.dom().root()).unwrap().children.len();
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

use crate::{background, button_secondary};
use yakui_core::geometry::{Constraints, Dim2, Vec2};
use yakui_core::{Alignment, CrossAxisAlignment, MainAxisAlignment, MainAxisSize, Pivot};
use yakui_widgets::widgets::{Layer, List, Pad};
use yakui_widgets::{colored_box_container, constrained, pad, reflow, use_state};

pub fn combo_box(selected: &mut usize, items: &[&str], w: f32) -> bool {
    let mut changed = false;

    constrained(Constraints::loose(Vec2::new(w, f32::INFINITY)), || {
        pad(Pad::horizontal(10.0), || {
            let mut l = List::column();
            l.main_axis_alignment = MainAxisAlignment::Center;
            l.main_axis_size = MainAxisSize::Min;
            l.cross_axis_alignment = CrossAxisAlignment::Stretch;
            l.show(|| {
                let open = use_state(|| false);

                if button_secondary(items[*selected].to_string())
                    .show()
                    .clicked
                {
                    open.modify(|x| !x);
                }

                if open.get() {
                    Layer::new().show(|| {
                        reflow(Alignment::BOTTOM_LEFT, Pivot::TOP_LEFT, Dim2::ZERO, || {
                            constrained(Constraints::loose(Vec2::new(w, f32::INFINITY)), || {
                                colored_box_container(background(), || {
                                    Pad::all(3.0).show(|| {
                                        let mut l = List::column();
                                        l.cross_axis_alignment = CrossAxisAlignment::Stretch;
                                        l.item_spacing = 3.0;
                                        l.show(|| {
                                            for (i, item) in items.iter().enumerate() {
                                                if button_secondary(item.to_string()).show().clicked
                                                {
                                                    open.set(false);
                                                    if i != *selected {
                                                        *selected = i;
                                                        changed = true;
                                                    }
                                                }
                                            }
                                        });
                                    });
                                });
                            });
                        });
                    });
                }
            });
        });
    });

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use yakui_core::event::Event;
    use yakui_core::geometry::Rect;
    use yakui_core::input::MouseButton;
    use yakui_core::{WidgetId, Yakui};
    use yakui_widgets::widgets::ColoredBoxWidget;

    #[test]
    fn p4d_combo_padding_and_option_gaps_block_world_input() {
        fn panel(ui: &Yakui, id: WidgetId) -> Option<Rect> {
            let node = ui.dom().get(id)?;
            if node.widget.is::<ColoredBoxWidget>() {
                return Some(ui.layout_dom().get(id)?.rect);
            }
            node.children.iter().find_map(|&child| panel(ui, child))
        }

        let mut ui = Yakui::new();
        let size = Vec2::new(400.0, 160.0);
        ui.set_surface_size(size);
        ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, size));
        let mut selected = 0;
        let mut frame = |ui: &mut Yakui| {
            ui.start();
            combo_box(&mut selected, &["Traffic", "Electricity"], 180.0);
            crate::render_tooltip();
            ui.finish();
            ui.paint();
        };
        frame(&mut ui);
        ui.handle_event(Event::CursorMoved(Some(Vec2::new(15.0, 12.0))));
        for down in [true, false] {
            ui.handle_event(Event::MouseButtonChanged {
                button: MouseButton::One,
                down,
            });
        }
        frame(&mut ui);
        let panel = panel(&ui, ui.dom().root()).expect("the dropdown is open");
        let padding = panel.pos() + Vec2::ONE;
        let gap = panel.pos() + panel.size() * 0.5;
        assert!(!crate::pointer_over_ui(panel.pos().x - 1.0, padding.y));
        assert!(crate::pointer_over_ui(padding.x, padding.y));
        assert!(crate::pointer_over_ui(gap.x, gap.y));
    }
}

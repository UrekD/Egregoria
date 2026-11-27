use yakui::widgets::{Layer, List};
use yakui::{
    colored_box_container, column, constrained, opaque, reflow, spacer, use_state, Alignment,
    Constraints, CrossAxisAlignment, Dim2, MainAxisSize, Pivot, Vec2,
};

use goryak::{
    background, blur_bg, button_primary, button_secondary, constrained_viewport, mincolumn, minrow,
    on_primary_container, on_secondary_container, padxy, secondary_container, textc, tooltip,
};
#[cfg(test)]
use simulation::economy::Government;
use simulation::Simulation;

use crate::gui::GuiState;
use crate::session::{self, Departure};
use crate::uiworld::{SaveLoadState, UiWorld};

#[cfg(test)]
pub fn menu_bar(uiworld: &UiWorld, sim: &Simulation) {
    let money = sim.read::<Government>().money;
    menu_bar_view(uiworld, Some(sim), money);
}

pub(super) fn menu_bar_view(uiworld: &UiWorld, sim: Option<&Simulation>, money: prototypes::Money) {
    profiling::scope!("hud::menu_bar");

    reflow(Alignment::TOP_LEFT, Pivot::TOP_LEFT, Dim2::ZERO, || {
        constrained_viewport(|| {
            column(|| {
                opaque(|| {
                    blur_bg(secondary_container().with_alpha(0.5), 0.0, || {
                        padxy(5.0, 5.0, || {
                            minrow(3.0, || {
                                let mut gui = uiworld.write::<GuiState>();
                                windows_dropdown(uiworld, sim, &mut gui);
                                if !session::world_interaction(uiworld) {
                                    return;
                                }
                                save_window(uiworld, sim);
                                textc(on_primary_container(), format!("Money: {money}"));
                                crate::gui::overlays::selector(uiworld, gui.show_unfinished_ui);
                            });
                        });
                    });
                });
                spacer(1);
            });
        });
    });
}

fn windows_dropdown(uiworld: &UiWorld, sim: Option<&Simulation>, gui: &mut GuiState) {
    mincolumn(0.0, || {
        let open = use_state(|| false);
        let toggle = tooltip("City windows and settings", || {
            button_primary("Windows v").show()
        });
        if toggle.clicked {
            open.modify(|open| !open);
        }
        if !open.get() {
            return;
        }

        let actions = || {
            let count_open = |gui: &mut GuiState| {
                gui.windows
                    .window_states()
                    .filter(|(_, open)| **open)
                    .count()
            };
            let previously_open = count_open(gui);
            gui.windows.menu(uiworld, sim);
            if session::world_interaction(uiworld) {
                gui.windows.additional_menu(false, gui.show_unfinished_ui);
            }
            if count_open(gui) != previously_open {
                open.set(false);
            }
        };

        Layer::new().show(|| {
            reflow(
                Alignment::BOTTOM_LEFT,
                Pivot::TOP_LEFT,
                Dim2::pixels(0.0, 3.0),
                || {
                    opaque(|| {
                        constrained(Constraints::loose(Vec2::new(150.0, f32::INFINITY)), || {
                            colored_box_container(background(), || {
                                padxy(3.0, 3.0, || {
                                    let mut items = List::column();
                                    items.main_axis_size = MainAxisSize::Min;
                                    items.cross_axis_alignment = CrossAxisAlignment::Stretch;
                                    items.item_spacing = 3.0;
                                    items.show(actions);
                                });
                            });
                        });
                    });
                },
            );
        });
    });
}

pub(super) fn main_menu(uiw: &UiWorld, settings_open: &mut bool, _: bool) {
    session::main_menu_buttons(uiw, None, settings_open);
}

pub(super) fn render_departure(uiw: &UiWorld) {
    let departure = uiw.read::<crate::session::SessionState>().departure;
    if let Some(departure) = departure {
        session::render_departure(uiw, departure);
    }
}

fn save_window(uiw: &UiWorld, sim: Option<&Simulation>) {
    if session::can_save(uiw) {
        let busy = {
            let io = uiw.read::<SaveLoadState>();
            io.saving()
        };
        if busy {
            textc(on_secondary_container(), "Saving…");
        } else if tooltip("Save the current city", || button_primary("Save").show()).clicked {
            session::request_save(uiw, None);
        }
    }
    if tooltip("Exit the game", || button_secondary("Exit").show()).clicked {
        session::request_departure_when_ready(uiw, sim, Departure::Exit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::hud::demand_bars;
    use crate::gui::hud::time_controls::time_controls;
    use simulation::PopulationScale;
    use yakui::font::{Font, FontSettings, Fonts};
    use yakui::widgets::{ButtonWidget, OpaqueWidget, TextWidget};
    use yakui::{Rect, WidgetId, Yakui};

    fn widget_rects<T: yakui::widget::Widget>(ui: &Yakui, id: WidgetId, rects: &mut Vec<Rect>) {
        let node = ui.dom().get(id).unwrap();
        if node.widget.is::<T>() {
            rects.push(ui.layout_dom().get(id).unwrap().rect);
        }
        for &child in &node.children {
            widget_rects::<T>(ui, child, rects);
        }
    }

    fn population_rect(ui: &Yakui, id: WidgetId, label: &str) -> Option<Rect> {
        let node = ui.dom().get(id).unwrap();
        if node
            .widget
            .downcast_ref::<TextWidget>()
            .is_some_and(|text| format!("{text:?}").contains(&format!("{label:?}")))
        {
            return Some(ui.layout_dom().get(id).unwrap().rect);
        }
        node.children
            .iter()
            .find_map(|&child| population_rect(ui, child, label))
    }

    fn assert_hud_fits(width: f32, height: f32, scale: PopulationScale) {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let mut ctx = scenarios::ScenarioCtx::new(simulation::SimulationOptions {
            terrain_size: 1,
            persons_per_agent: scale.persons_per_agent(),
            ..simulation::SimulationOptions::LEGACY
        });
        scenarios::builders::grid_city(&mut ctx, geom::vec2(150.0, 150.0), 2, 2, 160.0);
        let house = scenarios::builders::fill_houses(&mut ctx, 1)[0];
        assert!(simulation::souls::human::spawn_human(&mut ctx.sim, house).is_some());
        let sim = ctx.sim;
        sim.write::<Government>().money = prototypes::Money::MAX;
        let uiworld = UiWorld::with_defaults();
        uiworld.write::<session::SessionState>().active_slot =
            Some(common::saveload::SlotId::legacy());
        let mut ui = Yakui::new();
        let fonts = ui.dom().get_global_or_init(Fonts::default);
        for (name, bytes) in [
            (
                "icons",
                include_bytes!("../../../../assets/font_awesome_solid_900.otf").as_slice(),
            ),
            (
                "monospace",
                include_bytes!("../../../../assets/SpaceMono-Regular.ttf").as_slice(),
            ),
        ] {
            fonts.add(
                Font::from_bytes(bytes, FontSettings::default()).unwrap(),
                Some(name),
            );
        }
        let viewport = Vec2::new(width, height);
        ui.set_surface_size(viewport);
        ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));

        ui.start();
        column(|| {
            menu_bar(&uiworld, &sim);
            time_controls(&uiworld, &sim);
            demand_bars::render(&demand_bars::DemandSnapshot::capture(&sim));
        });
        ui.finish();

        let mut panels = Vec::new();
        widget_rects::<OpaqueWidget>(&ui, ui.dom().root(), &mut panels);
        let [menu, clock] = panels.as_slice() else {
            panic!("expected menu and clock panels, got {panels:?}");
        };
        assert!(
            menu.max().x < clock.pos().x,
            "menu {menu:?} overlaps clock {clock:?}"
        );
        assert!(clock.max().x <= width);
        let label = if scale == PopulationScale::REALISTIC {
            "Population 1".to_owned()
        } else {
            format!("Population 8 · {scale}")
        };
        let population = population_rect(&ui, ui.dom().root(), &label)
            .unwrap_or_else(|| panic!("missing HUD label {label:?}"));
        let area = Rect::from_pos_size(Vec2::ZERO, viewport);
        assert!(population.size().x > 0.0 && population.size().y > 0.0);
        assert!(
            area.contains_point(population.pos()) && area.contains_point(population.max()),
            "population {population:?} extends outside viewport {area:?}"
        );
        for panel in [menu, clock] {
            assert!(
                population.pos().y >= panel.max().y
                    || population.max().y <= panel.pos().y
                    || population.max().x <= panel.pos().x
                    || population.pos().x >= panel.max().x,
                "population {population:?} overlaps HUD panel {panel:?}"
            );
        }

        let mut buttons = Vec::new();
        widget_rects::<ButtonWidget>(&ui, ui.dom().root(), &mut buttons);
        assert!(buttons.len() > 4);
        for button in buttons {
            let panel = if button.pos().x < clock.pos().x {
                menu
            } else {
                clock
            };
            assert!(
                panel.contains_point(button.pos()) && panel.contains_point(button.max()),
                "button {button:?} extends outside its HUD panel {panel:?}"
            );
        }
    }

    #[test]
    fn ui1_hud_fits_at_1024() {
        assert_hud_fits(1024.0, 768.0, PopulationScale::REALISTIC);
    }

    #[test]
    fn population_scale_hud_fits_at_1024() {
        assert_hud_fits(1024.0, 768.0, PopulationScale::try_new(8).unwrap());
    }

    #[test]
    fn ui1_hud_fits_at_1280() {
        assert_hud_fits(1280.0, 720.0, PopulationScale::REALISTIC);
    }
}

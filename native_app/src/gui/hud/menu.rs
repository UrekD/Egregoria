use yakui::widgets::List;
use yakui::{column, opaque, reflow, spacer, Alignment, CrossAxisAlignment, Dim2, Pivot};

use goryak::{
    blur_bg, button_primary, button_secondary, constrained_viewport, on_primary_container,
    on_secondary_container, padxy, secondary_container, textc, tooltip,
};
use simulation::economy::Government;
use simulation::Simulation;

use crate::gui::GuiState;
use crate::session::{self, Departure};
use crate::uiworld::{SaveLoadState, UiWorld};

pub fn menu_bar(uiworld: &UiWorld, sim: &Simulation) {
    profiling::scope!("hud::menu_bar");

    reflow(Alignment::TOP_LEFT, Pivot::TOP_LEFT, Dim2::ZERO, || {
        constrained_viewport(|| {
            column(|| {
                opaque(|| {
                    blur_bg(secondary_container().with_alpha(0.5), 0.0, || {
                        padxy(5.0, 5.0, || {
                            let mut l = List::row();
                            l.item_spacing = 3.0;
                            l.cross_axis_alignment = CrossAxisAlignment::Center;

                            l.show(|| {
                                let mut gui = uiworld.write::<GuiState>();
                                gui.windows.menu(uiworld, sim);
                                if !session::world_interaction(uiworld) {
                                    return;
                                }
                                save_window(uiworld, sim);
                                textc(
                                    on_primary_container(),
                                    format!("Money: {}", sim.read::<Government>().money),
                                );
                                let show_unfinished_ui = gui.show_unfinished_ui;
                                crate::gui::overlays::selector(uiworld, show_unfinished_ui);
                                gui.windows.additional_menu(false, show_unfinished_ui);
                            });
                        });
                    });
                });
                spacer(1);
            });
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

fn save_window(uiw: &UiWorld, sim: &Simulation) {
    if session::can_save(uiw) {
        let busy = {
            let io = uiw.read::<SaveLoadState>();
            io.pending_save.is_some() || io.save_result.is_some()
        };
        if busy {
            textc(on_secondary_container(), "Saving…");
        } else if tooltip("Save the current city", || button_primary("Save").show()).clicked {
            session::request_save(uiw, None);
        }
    }
    if tooltip("Exit the game", || button_secondary("Exit").show()).clicked {
        session::request_departure(uiw, sim, Departure::Exit);
    }
}

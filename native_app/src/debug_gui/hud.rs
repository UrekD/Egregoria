use egui::{Color32, Context, Id, RichText};

use prototypes::Money;
use simulation::economy::Government;
use simulation::Simulation;

use crate::debug_gui::debug_inspect::debug_inspector;
use crate::debug_gui::debug_window::debug_window;
use crate::gui::{ErrorTooltip, GuiState, PotentialCommands};
use crate::inputmap::InputMap;
use crate::uiworld::UiWorld;

/// Root GUI entrypoint
pub fn render_oldgui(ui: &Context, uiworld: &UiWorld, sim: &Simulation) {
    profiling::scope!("hud::render");
    crate::gui::windows::load::replay_controls(ui, uiworld);
    if uiworld.read::<GuiState>().hidden {
        return;
    }

    let interactive = crate::session::world_interaction(uiworld);
    if interactive {
        debug_inspector(ui, uiworld, sim);
    }
    debug_window(ui, uiworld, sim);
    if interactive {
        tooltip(ui, uiworld, sim);
    }
}

pub fn tooltip(ui: &Context, uiworld: &UiWorld, sim: &Simulation) {
    profiling::scope!("gui::tooltip");
    let tooltip = std::mem::take(&mut *uiworld.write::<ErrorTooltip>());
    if let Some(msg) = tooltip.msg {
        if !(tooltip.isworld && ui.is_pointer_over_area()) {
            let pointer = uiworld.read::<InputMap>().screen;
            let pos = egui::pos2(pointer.x, pointer.y) / ui.pixels_per_point();
            let anchor = egui::Rect::from_center_size(pos, egui::vec2(30.0, 30.0));
            egui::show_tooltip_for(ui, Id::new("tooltip_error"), &anchor, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(msg).color(Color32::from_rgb(255, 100, 100)))
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
        }
    }

    if ui.is_pointer_over_area() {
        return;
    }
    let pot = &mut uiworld.write::<PotentialCommands>().0;
    let cost: Money = pot
        .drain(..)
        .map(|cmd| Government::action_cost(&cmd, sim))
        .sum();

    if cost == Money::ZERO {
        return;
    }

    egui::show_tooltip(ui, Id::new("tooltip_command_cost"), |ui| {
        if cost > sim.read::<Government>().money {
            ui.colored_label(Color32::RED, format!("{cost} too expensive"));
        } else {
            ui.label(cost.to_string());
        }
    });
}

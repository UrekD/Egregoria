use egui::{Color32, Context, Id, RichText};
use geom::{Ray3, Vec2};
use std::time::{Duration, Instant};

use prototypes::Money;
use simulation::economy::Government;
use simulation::Simulation;

use crate::debug_gui::debug_inspect::debug_inspector;
use crate::debug_gui::debug_window::debug_window;
use crate::gui::{inspect, selectable, ErrorTooltip, GuiState, PotentialCommands, Tool};
use crate::inputmap::InputMap;
use crate::uiworld::UiWorld;

#[derive(Default)]
pub(crate) struct VehicleHover {
    sample: Option<(Instant, Vec2, Ray3, Option<String>)>,
}

impl VehicleHover {
    fn text(&mut self, sim: &Simulation, input: &InputMap) -> Option<&str> {
        let ray = input.ray?;
        let now = Instant::now();
        if self
            .sample
            .as_ref()
            .is_none_or(|(at, _, _, _)| now.duration_since(*at) >= Duration::from_millis(100))
        {
            let text = selectable::pick_vehicle(sim, input)
                .and_then(|id| inspect::vehicle_tooltip(sim, id));
            self.sample = Some((now, input.screen, ray, text));
        }
        let (_, screen, sampled_ray, text) = self.sample.as_ref()?;
        (*screen == input.screen && sampled_ray.from == ray.from && sampled_ray.dir == ray.dir)
            .then_some(text.as_deref())
            .flatten()
    }
}

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
    let show_vehicle = tooltip.msg.is_none();
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
    if show_vehicle && matches!(*uiworld.read::<Tool>(), Tool::Hand | Tool::RoadEditor) {
        let input = uiworld.read::<InputMap>();
        if !goryak::pointer_over_ui(input.screen.x, input.screen.y) {
            let mut hover = uiworld.write::<VehicleHover>();
            if let Some(text) = hover.text(sim, &input) {
                let pos = egui::pos2(input.screen.x, input.screen.y) / ui.pixels_per_point();
                egui::Area::new(Id::new("tooltip_vehicle"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(pos + egui::vec2(12.0, 16.0))
                    .interactable(false)
                    .show(ui, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(text);
                        });
                    });
            }
        }
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

use egui::{Color32, Context, Id, RichText};
use std::time::{Duration, Instant};

use prototypes::Money;
use simulation::economy::Government;
use simulation::Simulation;

use crate::debug_gui::debug_inspect::debug_inspector;
use crate::debug_gui::debug_window::debug_window;
use crate::gui::{inspect, selectable, ErrorTooltip, GuiState, PotentialCommands, Tool};
use crate::inputmap::InputMap;
use crate::uiworld::UiWorld;

const HOVER_REFRESH: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct VehicleHover {
    hit: Option<selectable::VehicleHit>,
    last_pick: Option<Instant>,
    text: Option<(Instant, simulation::AnyEntity, String)>,
}

impl VehicleHover {
    pub(crate) fn update(
        &mut self,
        picker: &mut selectable::VehiclePicker,
        sim: &Simulation,
        input: &InputMap,
        now: Instant,
        collect_instances: impl FnOnce(Option<&mut selectable::VehiclePicker>),
    ) {
        if self.hit.is_none()
            && input.ray.is_some()
            && self
                .last_pick
                .is_some_and(|at| now.duration_since(at) < HOVER_REFRESH)
        {
            collect_instances(None);
            return;
        }

        let map = sim.map();
        let Some(ray) = selectable::PickRay::from_input(&map, input) else {
            *self = Self::default();
            collect_instances(None);
            return;
        };
        self.hit = self
            .hit
            .filter(|&hit| picker.still_visible(sim, &map, ray, hit));
        let acquire = self.hit.is_none();
        collect_instances(acquire.then_some(&mut *picker));
        if acquire {
            self.last_pick = Some(now);
            self.hit = picker.pick_indexed(sim, &map, ray);
        }
    }

    fn text_at(&mut self, uiworld: &UiWorld, sim: &Simulation, now: Instant) -> Option<&str> {
        let Some(hit) = self.hit else {
            self.text = None;
            return None;
        };
        if self.text.as_ref().is_none_or(|(at, entity, _)| {
            *entity != hit.entity || now.duration_since(*at) >= HOVER_REFRESH
        }) {
            self.text = inspect::vehicle_tooltip(uiworld, sim, hit.entity)
                .map(|text| (now, hit.entity, text));
        }
        self.text.as_ref().map(|(_, _, text)| text.as_str())
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
            if let Some(text) = hover.text_at(uiworld, sim, Instant::now()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec3, Ray3, Transform, Vec3};
    use prototypes::GameTime;
    use scenarios::ScenarioCtx;
    use simulation::transportation::{spawn_driving_vehicle, VehicleKind};
    use simulation::SimulationOptions;

    fn sample_hover<'a>(
        hover: &'a mut VehicleHover,
        ui: &UiWorld,
        sim: &Simulation,
        input: &InputMap,
        now: Instant,
    ) -> Option<&'a str> {
        let mut picker = ui.write::<selectable::VehiclePicker>();
        hover.update(&mut picker, sim, input, now, |picker| {
            if let Some(picker) =
                picker.and_then(|picker| picker.start_snapshot(sim).then_some(picker))
            {
                for (id, vehicle) in &sim.world().vehicles {
                    picker.index_road(id, vehicle);
                }
                for (id, wagon) in &sim.world().wagons {
                    picker.index_wagon(id, wagon);
                }
            }
        });
        drop(picker);
        hover.text_at(ui, sim, now)
    }

    #[test]
    fn vehicle_destination_round2_empty_hover_does_not_scan_each_new_tick() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let position = vec3(80.0, 80.0, 100.0);
        let car = spawn_driving_vehicle(&mut ctx.sim, VehicleKind::Car, Transform::new(position))
            .unwrap();
        let mut input = InputMap::default();
        input.ray = Some(Ray3::new(vec3(150.0, 150.0, 150.0), -Vec3::Z));
        let now = Instant::now();
        let mut hover = VehicleHover::default();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        selectable::VEHICLE_SCAN_COUNT.with(|count| count.set(0));
        for frame in 0..3 {
            ctx.sim.write::<GameTime>().tick.0 += 1;
            if frame == 2 {
                // A car can move under the stationary cursor between hover refreshes.
                ctx.sim.world_mut_unchecked().vehicles[car].trans.pos = vec3(150.0, 150.0, 100.0);
            }
            let text = sample_hover(
                &mut hover,
                &ui,
                &ctx.sim,
                &input,
                now + Duration::from_millis(frame * 101),
            );
            assert_eq!(text.is_some(), frame == 2);
        }
        selectable::VEHICLE_SCAN_COUNT.with(|count| assert_eq!(count.get(), 0));
    }

    #[test]
    fn vehicle_destination_hover_tracks_camera_between_text_samples() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let pos = vec3(100.0, 100.0, 100.0);
        let id =
            spawn_driving_vehicle(&mut ctx.sim, VehicleKind::Car, Transform::new(pos)).unwrap();
        let mut input = InputMap::default();
        input.ray = Some(Ray3::new(pos.up(24.0), -Vec3::Z));
        let now = Instant::now();
        let mut hover = VehicleHover::default();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        let text = sample_hover(&mut hover, &ui, &ctx.sim, &input, now)
            .unwrap()
            .to_owned();

        ctx.sim.world_mut_unchecked().vehicles[id].speed.0 = 10.0;
        ui.write::<inspect::VehicleRouteFrame>().clear();
        input.ray = Some(Ray3::new(pos.up(23.0) + Vec3::X, -Vec3::Z));
        assert_eq!(
            sample_hover(
                &mut hover,
                &ui,
                &ctx.sim,
                &input,
                now + Duration::from_millis(16)
            ),
            Some(text.as_str()),
        );
        assert_ne!(
            sample_hover(
                &mut hover,
                &ui,
                &ctx.sim,
                &input,
                now + Duration::from_millis(101)
            ),
            Some(text.as_str()),
        );

        input.ray = Some(Ray3::new(pos.up(24.0) + Vec3::X * 20.0, -Vec3::Z));
        assert!(sample_hover(
            &mut hover,
            &ui,
            &ctx.sim,
            &input,
            now + Duration::from_millis(102)
        )
        .is_none());
    }
}

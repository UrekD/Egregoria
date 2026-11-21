use crate::map::{Intersection, LaneID, Lanes, Roads, TrafficControl, TrafficLightSchedule};
use egui_inspect::{egui, egui::Ui, Inspect, InspectArgs};
use prototypes::SECONDS_PER_REALTIME_SECOND;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightPolicy {
    NoLights,
    StopSigns,
    Lights,
    #[default]
    Auto,
}

impl LightPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoLights => "No lights",
            Self::StopSigns => "Stop signs",
            Self::Lights => "Lights",
            Self::Auto => "Auto",
        }
    }

    pub fn apply(self, inter: &Intersection, lanes: &mut Lanes, roads: &Roads) {
        let in_road_lanes: Vec<Vec<LaneID>> = inter
            .roads
            .iter()
            .map(|&x| {
                roads
                    .get(x)
                    .into_iter()
                    .flat_map(|r| {
                        r.incoming_lanes_to(inter.id)
                            .iter()
                            .filter(|(_, kind)| kind.needs_light())
                            .map(|&(id, _)| id)
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .collect();

        for incoming_lanes in &in_road_lanes {
            for &lane in incoming_lanes {
                unwrap_cont!(lanes.get_mut(lane)).control = TrafficControl::Always;
            }
        }

        match self {
            LightPolicy::NoLights => {}
            LightPolicy::StopSigns => {
                Self::stop_signs(in_road_lanes, lanes);
            }
            LightPolicy::Lights => {
                Self::lights(in_road_lanes, inter, lanes);
            }
            LightPolicy::Auto => {
                if in_road_lanes.len() <= 2 {
                    return;
                }
                if in_road_lanes.len() == 3 {
                    Self::stop_signs(in_road_lanes, lanes);
                    return;
                }

                if inter.turn_policy.left_turns {
                    Self::lights(in_road_lanes, inter, lanes);
                } else {
                    Self::stop_signs(in_road_lanes, lanes);
                }
            }
        }
    }

    pub fn is_stop_signs(&self) -> bool {
        matches!(self, LightPolicy::StopSigns)
    }

    fn stop_signs(in_road_lanes: Vec<Vec<LaneID>>, lanes: &mut Lanes) {
        for incoming_lanes in in_road_lanes {
            for lane in incoming_lanes {
                unwrap_cont!(lanes.get_mut(lane)).control = TrafficControl::StopSign;
            }
        }
    }

    fn lights(in_road_lanes: Vec<Vec<LaneID>>, inter: &Intersection, lanes: &mut Lanes) {
        let n_cycles = ((in_road_lanes.len() + 1) / 2) as u16;
        let cycle_size = 14 * SECONDS_PER_REALTIME_SECOND as u16;
        let orange_length = 4 * SECONDS_PER_REALTIME_SECOND as u16;

        let total_length = cycle_size * n_cycles;

        let inter_offset =
            (common::rand::rand(inter.id.as_ffi() as f32) * total_length as f32) as u16;

        for (i, incoming_lanes) in in_road_lanes.into_iter().enumerate() {
            let i = i as u16;
            let light = TrafficControl::Light(TrafficLightSchedule::from_basic(
                cycle_size - orange_length,
                orange_length,
                total_length - cycle_size,
                cycle_size * (i % n_cycles) + inter_offset,
            ));

            for lane in incoming_lanes {
                unwrap_cont!(lanes.get_mut(lane)).control = light;
            }
        }
    }
}

impl Inspect<LightPolicy> for LightPolicy {
    fn render(data: &LightPolicy, label: &'static str, ui: &mut Ui, _: &InspectArgs) {
        if label.is_empty() {
            ui.label(data.label());
        } else {
            ui.label(format!("{label}: {}", data.label()));
        }
    }

    fn render_mut(
        data: &mut LightPolicy,
        label: &'static str,
        ui: &mut Ui,
        _: &InspectArgs,
    ) -> bool {
        let policies = [Self::NoLights, Self::StopSigns, Self::Lights, Self::Auto];
        let mut index = policies.iter().position(|policy| policy == data).unwrap();
        let changed = egui::ComboBox::from_label(label)
            .show_index(ui, &mut index, policies.len(), |i| policies[i].label())
            .changed();
        if changed {
            *data = policies[index];
        }

        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_policy_read_only_inspector_renders_every_policy() {
        let policies = [
            LightPolicy::NoLights,
            LightPolicy::StopSigns,
            LightPolicy::Lights,
            LightPolicy::Auto,
        ];
        let before = policies;
        let output = egui::Context::default().run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for policy in &policies {
                    LightPolicy::render(policy, "", ui, &InspectArgs::default());
                }
            });
        });

        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["No lights", "Stop signs", "Lights", "Auto"]);
        assert_eq!(policies, before);
    }
}

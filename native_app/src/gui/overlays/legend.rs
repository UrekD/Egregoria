use goryak::{
    blur_bg, fixed_spacer, mincolumn, minrow, on_secondary_container, padxy, secondary_container,
    textc,
};
use prototypes::{
    try_prototype, GoodsCompanyID, Power, ServiceID, DEFAULT_WATER_SERVICE_DISTANCE_M,
};
use simulation::map_dynamic::UtilityNetworkState;
use yakui::{
    colored_box_container, constrained, reflow, Alignment, Color, Constraints, Dim2, Pivot, Vec2,
    Vec4,
};

use super::{ground, placement_overlay, OverlayMode};
use crate::gui::{PlacementPreview, Tool};
use crate::uiworld::UiWorld;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Scale {
    pub(super) labels: [&'static str; 2],
    maximum: f32,
    colors: [[f32; 3]; 3],
    opacity: f32,
}

const SERVICE_COLORS: [[f32; 3]; 3] = [[0.85, 0.07, 0.09], [0.98, 0.55, 0.03], [0.03, 0.80, 0.38]];
const COVERAGE_COLORS: [[f32; 3]; 3] = [
    [0.80, 0.012, 0.018],
    [0.96, 0.43, 0.015],
    [0.01, 0.68, 0.075],
];
pub(super) const COVERAGE_PROVIDER_COLOR: [f32; 4] = [0.025, 0.45, 1.0, 0.96];
pub(super) const INACTIVE_PROVIDER_COLOR: [f32; 4] = [0.47, 0.48, 0.57, 0.95];
pub(super) const REACH_COLOR: [f32; 4] = [0.02, 0.76, 0.20, 0.55];
pub(super) const MAX_REACH_COLOR: [f32; 4] = [0.05, 0.68, 0.9, 0.72];
pub(crate) const UTILITY_EXPLANATION: &str = "Roads carry electricity, water and sewage. Buildings on the same connected road network share each utility's capacity; a pump needs power.";

pub(super) fn utility_color(state: UtilityNetworkState) -> [f32; 4] {
    match state {
        UtilityNetworkState::Idle => [0.48, 0.52, 0.55, 0.72],
        UtilityNetworkState::Disconnected => [0.31, 0.33, 0.43, 0.80],
        UtilityNetworkState::Updating => [0.82, 0.66, 0.07, 0.84],
        UtilityNetworkState::Shortage => [0.95, 0.16, 0.10, 0.85],
        UtilityNetworkState::Ok => [0.035, 0.81, 0.46, 0.80],
    }
}

impl Scale {
    pub(super) fn color(self, value: f32) -> [f32; 4] {
        let t = (value / self.maximum).clamp(0.0, 1.0);
        let (from, to, weight) = if t <= 0.5 {
            (self.colors[0], self.colors[1], 2.0 * t)
        } else {
            (self.colors[1], self.colors[2], 2.0 * t - 1.0)
        };
        let channel = |i: usize| from[i] + (to[i] - from[i]) * weight;
        [channel(0), channel(1), channel(2), self.opacity]
    }
}

impl OverlayMode {
    pub(super) fn scale(self) -> Option<Scale> {
        use OverlayMode::*;

        let labels = match self {
            Off | Transit | Traffic => return None,
            Power => ["Blackout / disconnected", "Powered"],
            Water => ["No water", "Supplied"],
            Sewage => ["No sewage service", "Served"],
            Coverage(_) => ["0%", "100%"],
            Pollution => ["0", "100+"],
            LandValue | Happiness => ["0", "100"],
        };
        let maximum = match self {
            Power | Water | Sewage => 1.0,
            _ => 100.0,
        };
        let colors = match self {
            Coverage(_) => COVERAGE_COLORS,
            Pollution => [[0.08, 0.23, 0.65], [0.55, 0.06, 0.62], [0.98, 0.19, 0.02]],
            _ => SERVICE_COLORS,
        };
        let opacity = if matches!(self, Coverage(_)) {
            0.94
        } else {
            0.62
        };
        Some(Scale {
            labels,
            maximum,
            colors,
            opacity,
        })
    }
}

pub(super) fn render(mode: OverlayMode, label: &str, empty: bool, uiw: &UiWorld) {
    if mode == OverlayMode::Traffic {
        super::traffic::legend(uiw);
        return;
    }
    if mode == OverlayMode::Off {
        return;
    }
    let Some(scale) = mode.scale() else { return };
    let placement =
        (*uiw.read::<Tool>() == Tool::SpecialBuilding).then(|| uiw.read::<PlacementPreview>());
    let placement = placement
        .as_deref()
        .filter(|preview| preview.company.and_then(placement_overlay) == Some(mode));

    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(12.0, 112.0),
        || {
            blur_bg(secondary_container().with_alpha(0.94), 8.0, || {
                padxy(10.0, 6.0, || {
                    mincolumn(4.0, || {
                        textc(on_secondary_container(), label.to_owned());
                        if mode.is_utility() {
                            utility_legend(mode, placement.and_then(|preview| preview.company));
                            return;
                        }
                        minrow(0.0, || {
                            for step in 0..=10 {
                                swatch(scale, step as f32 * scale.maximum / 10.0, 16.0);
                            }
                        });
                        minrow(8.0, || {
                            textc(on_secondary_container(), scale.labels[0]);
                            fixed_spacer((120.0, 0.0));
                            textc(on_secondary_container(), scale.labels[1]);
                        });
                        match mode {
                            OverlayMode::Coverage(_) => {
                                legend_item(REACH_COLOR, "Served area");
                                legend_item(MAX_REACH_COLOR, "Max reach (ring)");
                                legend_item(COVERAGE_PROVIDER_COLOR, "Active provider");
                                legend_item(INACTIVE_PROVIDER_COLOR, "Inactive provider");
                                textc(
                                    on_secondary_container(),
                                    "Building colors show actual service",
                                );
                            }
                            OverlayMode::Pollution if empty => {
                                textc(on_secondary_container(), "No pollution recorded");
                            }
                            OverlayMode::LandValue | OverlayMode::Happiness => {
                                textc(on_secondary_container(), "Estimated between buildings");
                                if empty {
                                    textc(on_secondary_container(), "No building scores yet");
                                }
                            }
                            _ => {}
                        }
                        if mode == OverlayMode::Pollution
                            && placement.and_then(ground::preview_disc).is_some()
                        {
                            textc(on_secondary_container(), "Potential at full operation");
                        }
                    });
                });
            });
        },
    );
}

fn utility_legend(mode: OverlayMode, company: Option<GoodsCompanyID>) {
    use UtilityNetworkState::{Disconnected, Idle, Ok, Shortage, Updating};
    for states in [&[Ok, Shortage, Idle][..], &[Updating, Disconnected]] {
        minrow(12.0, || {
            for &state in states {
                legend_item(utility_color(state), state.label());
            }
        });
    }
    legend_item(COVERAGE_PROVIDER_COLOR, "Provider");
    constrained(Constraints::loose(Vec2::new(335.0, f32::INFINITY)), || {
        textc(on_secondary_container(), UTILITY_EXPLANATION);
    });
    let Some(prototype) = company.and_then(try_prototype) else {
        return;
    };
    let shore = match mode {
        OverlayMode::Water => Some("River or lake shore"),
        OverlayMode::Sewage => Some("Downstream river bank or coast"),
        _ => None,
    };
    if let Some((shore, service)) =
        shore.zip(company.and_then(|id| try_prototype(ServiceID::from(id))))
    {
        let reach = service
            .max_water_distance_m
            .unwrap_or(DEFAULT_WATER_SERVICE_DISTANCE_M);
        textc(
            on_secondary_container(),
            format!("{shore} · {reach} m reach on water maps"),
        );
    }
    let network = ground::placement_network();
    if let Some(network) = network.as_ref() {
        let state = mode.utility_state(network);
        let line = match mode {
            OverlayMode::Power => network.power_totals.as_ref().map(|totals| {
                format!(
                    "{} supply / {} demand",
                    totals.produced_power, totals.consumed_power
                )
            }),
            OverlayMode::Water => network.utility_totals.map(|totals| {
                format!(
                    "{:.0} supply / {:.0} demand",
                    totals.water_production, totals.water_demand
                )
            }),
            OverlayMode::Sewage => network.utility_totals.map(|totals| {
                format!(
                    "{:.0} capacity / {:.0} demand",
                    totals.sewage_capacity, totals.sewage_demand
                )
            }),
            _ => None,
        };
        legend_item(utility_color(state), "Placement road network");
        textc(
            on_secondary_container(),
            format!("Current network · {}", state.label()),
        );
        if let Some(line) = line {
            textc(on_secondary_container(), line);
        }
        if mode == OverlayMode::Water && network.electricity != Ok {
            textc(
                on_secondary_container(),
                format!(
                    "Pump needs power · electricity {}",
                    network.electricity.label()
                ),
            );
        }
    }
    let output = match mode {
        OverlayMode::Power => {
            let mut output = prototype.power_production.unwrap_or(Power::ZERO);
            if prototype.zone.is_some() {
                let area = prototype.size.w * prototype.size.h;
                output = output * (f64::from(area) / f64::from(simulation::map::MAX_ZONE_AREA));
            }
            format!("Would supply up to {output}")
        }
        OverlayMode::Water => {
            format!(
                "Would supply up to {:.0} water with power",
                prototype.water_production.unwrap_or_default()
            )
        }
        OverlayMode::Sewage => {
            format!(
                "Would add up to {:.0} sewage capacity",
                prototype.sewage_capacity.unwrap_or_default()
            )
        }
        _ => return,
    };
    textc(on_secondary_container(), output);
}

pub(super) fn legend_item(color: [f32; 4], label: &'static str) {
    minrow(4.0, || {
        color_swatch(color, 12.0);
        textc(on_secondary_container(), label);
    });
}

fn swatch(scale: Scale, value: f32, width: f32) {
    color_swatch(scale.color(value), width);
}

fn color_swatch([r, g, b, _]: [f32; 4], width: f32) {
    colored_box_container(Color::from_linear(Vec4::new(r, g, b, 1.0)), || {
        fixed_spacer((width, 9.0));
    });
}

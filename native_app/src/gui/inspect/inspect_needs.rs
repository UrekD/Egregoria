use goryak::{error, fixed_spacer, on_secondary_container, textc};
use prototypes::ServiceKind;
use simulation::diagnostics::{BuildingDiagnostics, PowerStatus};
use simulation::map::{BuildingID, BuildingKind};
use simulation::map_dynamic::{CoverageGrid, EnvGrid, UtilityStatus};
use simulation::Simulation;

use crate::gui::overlays::is_coverage_receiver;

pub(super) fn render(sim: &Simulation, building: BuildingID, diagnostics: &BuildingDiagnostics) {
    let list = needs(sim, building, diagnostics);
    if list.is_empty() {
        return;
    }

    fixed_spacer((0.0, 5.0));
    textc(on_secondary_container(), "Needs");
    for need in list {
        match need.met {
            Some(true) => textc(
                on_secondary_container(),
                format!("{} · Met · {}", need.label, need.detail),
            ),
            Some(false) => textc(error(), format!("{} · Unmet · {}", need.label, need.detail)),
            None => textc(
                on_secondary_container(),
                format!("{} · {}", need.label, need.detail),
            ),
        }
    }
    let requirements = &diagnostics.upgrade_requirements;
    if !requirements.is_empty() {
        fixed_spacer((0.0, 5.0));
        textc(on_secondary_container(), "Upgrade requirements");
        for requirement in requirements {
            let (color, status) = if requirement.met {
                (on_secondary_container(), "Met")
            } else {
                (error(), "Unmet")
            };
            let unit = if requirement.name.ends_with(" coverage")
                || matches!(requirement.name, "Occupancy" | "Employment")
            {
                "%"
            } else {
                ""
            };
            textc(
                color,
                format!(
                    "{} · {status} · {:.1}{unit} / {:.0}{unit}",
                    requirement.name, requirement.value, requirement.threshold
                ),
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Need {
    label: &'static str,
    met: Option<bool>,
    detail: String,
}

fn needs(sim: &Simulation, id: BuildingID, diagnostics: &BuildingDiagnostics) -> Vec<Need> {
    let map = sim.map();
    let Some(building) = map.get(id) else {
        return Vec::new();
    };
    let receiver = is_coverage_receiver(building.kind);
    let utilities = diagnostics.utilities;
    if !receiver && utilities.is_none() {
        return Vec::new();
    }

    let mut list = Vec::new();
    list.push(Need {
        label: "Power",
        met: Some(diagnostics.power == PowerStatus::Available),
        detail: power_detail(diagnostics.power).into(),
    });

    if let Some(retail) = diagnostics.retail_access {
        list.push(Need {
            label: "Retail access",
            met: Some(retail),
            detail: if retail {
                "Store in range"
            } else {
                "No store in range"
            }
            .into(),
        });
    }

    if let Some(utilities) = utilities {
        for (label, status, capacity_demand) in [
            (
                "Water",
                utilities.water,
                utilities
                    .totals
                    .map(|t| (t.water_production, t.water_demand)),
            ),
            (
                "Sewage",
                utilities.sewage,
                utilities
                    .totals
                    .map(|t| (t.sewage_capacity, t.sewage_demand)),
            ),
        ] {
            let reason = match status {
                UtilityStatus::Served => "Available",
                UtilityStatus::Shortage
                    if capacity_demand.is_some_and(|(capacity, _)| capacity <= 0.0) =>
                {
                    "No active supply"
                }
                UtilityStatus::Shortage => "Network shortage",
                UtilityStatus::Disconnected => "Disconnected",
                UtilityStatus::Unknown => "Awaiting network refresh",
            };
            let detail = capacity_demand.map_or_else(
                || reason.into(),
                |(capacity, demand)| {
                    format!("{reason} ({capacity:.1} capacity / {demand:.1} demand)")
                },
            );
            list.push(Need {
                label,
                met: Some(status == UtilityStatus::Served),
                detail,
            });
        }
    }

    if receiver {
        let coverage = sim.read::<CoverageGrid>();
        for (kind, label) in [
            (ServiceKind::School, "School coverage"),
            (ServiceKind::Health, "Health coverage"),
            (ServiceKind::Fire, "Fire coverage"),
            (ServiceKind::Police, "Police coverage"),
            (ServiceKind::Garbage, "Garbage coverage"),
            (ServiceKind::Park, "Park coverage"),
        ] {
            list.push(Need {
                label,
                met: None,
                detail: format!("{:.0}%", coverage.coverage(id, kind)),
            });
        }

        let env = sim.read::<EnvGrid>();
        list.extend([
            Need {
                label: "Land value",
                met: None,
                detail: format!("{} / 100", env.land_value(id)),
            },
            Need {
                label: "Pollution",
                met: None,
                detail: format!("{:.1}", env.pollution_at(building.obb.center())),
            },
        ]);
    }

    if building.kind == BuildingKind::House {
        list.push(Need {
            label: "Happiness",
            met: None,
            detail: diagnostics.happiness.map_or_else(
                || "Awaiting first score".into(),
                |score| format!("{score} / 100"),
            ),
        });
    }
    list
}

pub(super) fn power_detail(power: PowerStatus) -> &'static str {
    match power {
        PowerStatus::MissingNetwork => "No network connection",
        PowerStatus::Available => "Network powered",
        PowerStatus::Blackout => "Network blackout",
    }
}

#[cfg(test)]
mod tests {
    use geom::vec2;
    use scenarios::{
        builders::{fill_houses, grid_city},
        ScenarioCtx as TestCtx,
    };
    use simulation::SimulationOptions;

    use super::*;

    fn diagnosed_needs(sim: &Simulation, id: BuildingID) -> Vec<Need> {
        let diagnostics = simulation::diagnostics::building_diagnostics(sim, id).unwrap();
        needs(sim, id, &diagnostics)
    }

    #[test]
    fn p2e_fix_orphaned_house_reports_disconnection_immediately() {
        use scenarios::builders::CommandSink;
        use simulation::world_command::WorldCommand;

        let mut test = TestCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        let road = test.sim.map().get(house).unwrap().connected_road.unwrap();
        test.submit(&[WorldCommand::MapRemoveRoad(road)]);
        test.flush();
        let list = diagnosed_needs(&test.sim, house);
        for label in ["Water", "Sewage"] {
            let need = list.iter().find(|need| need.label == label).unwrap();
            assert_eq!(need.met, Some(false));
            assert_eq!(need.detail, "Disconnected");
        }
    }

    #[test]
    fn p2e_needs_read_live_residential_snapshots() {
        let mut test = TestCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        test.sim
            .write::<simulation::souls::happiness::Happiness>()
            .by_building
            .insert(house, 63);

        let list = diagnosed_needs(&test.sim, house);
        let find = |label| list.iter().find(|need| need.label == label).unwrap();
        assert_eq!(find("Power").met, Some(false));
        assert_eq!(find("Power").detail, "Network blackout");
        assert_eq!(find("Water").met, Some(false));
        assert_eq!(find("Sewage").met, Some(false));
        assert_eq!(find("School coverage").detail, "0%");
        assert_eq!(find("Happiness").detail, "63 / 100");
    }
}

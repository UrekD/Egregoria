use goryak::{error, fixed_spacer, on_secondary_container, textc};
use prototypes::ServiceKind;
use simulation::map::{BuildingID, BuildingKind};
use simulation::map_dynamic::{CoverageGrid, ElectricityFlow, EnvGrid, UtilityFlow};
use simulation::souls::happiness::Happiness;
use simulation::Simulation;

use crate::gui::overlays::{is_coverage_receiver, power_available, uses_water};

pub(super) fn render(sim: &Simulation, building: BuildingID) {
    let list = needs(sim, building);
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
    if sim
        .map()
        .get(building)
        .is_some_and(|b| b.kind == BuildingKind::House)
    {
        textc(on_secondary_container(), "requirements: see P2d");
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Need {
    label: &'static str,
    met: Option<bool>,
    detail: String,
}

fn needs(sim: &Simulation, id: BuildingID) -> Vec<Need> {
    let map = sim.map();
    let Some(building) = map.get(id) else {
        return Vec::new();
    };
    let receiver = is_coverage_receiver(building.kind);
    if !receiver && !uses_water(building) {
        return Vec::new();
    }

    let power = sim.read::<ElectricityFlow>();
    let utilities = sim.read::<UtilityFlow>();
    let network = map
        .electricity
        .net_id(id)
        .filter(|_| !building.is_orphaned());
    let power_met = power_available(&map, &power, id);
    let mut list = Vec::new();
    let power_detail = match network {
        None => "No network connection",
        Some(_) if power_met => "Network powered",
        Some(_) => "Network blackout",
    };
    list.push(Need {
        label: "Power",
        met: Some(power_met),
        detail: power_detail.into(),
    });

    let totals = network.and_then(|network| {
        utilities
            .networks()
            .find_map(|(id, totals)| (id == network).then_some(totals))
    });
    for (label, met, capacity_demand) in [
        (
            "Water",
            utilities.has_water(id),
            totals.map(|t| (t.water_production, t.water_demand)),
        ),
        (
            "Sewage",
            utilities.has_sewage(id),
            totals.map(|t| (t.sewage_capacity, t.sewage_demand)),
        ),
    ] {
        let detail = match capacity_demand {
            Some((capacity, demand)) => {
                let reason = if met {
                    "Available"
                } else if capacity <= 0.0 {
                    "No active supply"
                } else if capacity < demand {
                    "Network shortage"
                } else {
                    "Awaiting network refresh"
                };
                format!("{reason} ({capacity:.1} capacity / {demand:.1} demand)")
            }
            None if network.is_some() => "Awaiting network refresh".into(),
            None => "No network connection".into(),
        };
        list.push(Need {
            label,
            met: Some(met),
            detail,
        });
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
        let happiness = sim.read::<Happiness>();
        list.push(Need {
            label: "Happiness",
            met: None,
            detail: happiness.by_building.get(&id).map_or_else(
                || "Awaiting first score".into(),
                |score| format!("{score} / 100"),
            ),
        });
    }
    list
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

    #[test]
    fn p2e_needs_read_live_residential_snapshots() {
        let mut test = TestCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        test.sim
            .write::<simulation::souls::happiness::Happiness>()
            .by_building
            .insert(house, 63);

        let list = needs(&test.sim, house);
        let find = |label| list.iter().find(|need| need.label == label).unwrap();
        assert_eq!(find("Power").met, Some(false));
        assert_eq!(find("Power").detail, "Network blackout");
        assert_eq!(find("Water").met, Some(false));
        assert_eq!(find("Sewage").met, Some(false));
        assert_eq!(find("School coverage").detail, "0%");
        assert_eq!(find("Happiness").detail, "63 / 100");
    }
}

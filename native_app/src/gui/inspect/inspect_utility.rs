use goryak::{mincolumn, on_secondary_container, textc, tooltip};
use simulation::map::{BuildingID, RoadID};
use simulation::map_dynamic::{utility_network_at_road, UtilityNetworkState};

use crate::gui::overlays::UTILITY_EXPLANATION;
use simulation::Simulation;

pub(crate) fn render(sim: &Simulation, road: RoadID) {
    let Some(network) = utility_network_at_road(sim, road) else {
        return;
    };
    mincolumn(3.0, || {
        heading();
        let line = |name, state: UtilityNetworkState, totals: Option<String>| {
            let summary = format!("{name} · {}", state.label());
            let text = match totals {
                Some(totals) => format!("{summary} · {totals}"),
                None => summary,
            };
            textc(on_secondary_container(), text);
        };
        line(
            "Electricity",
            network.electricity,
            network.power_totals.map(|totals| {
                format!(
                    "{} supply / {} demand",
                    totals.produced_power, totals.consumed_power
                )
            }),
        );
        line(
            "Water",
            network.water,
            network.utility_totals.map(|totals| {
                format!(
                    "{:.1} supply / {:.1} demand",
                    totals.water_production, totals.water_demand
                )
            }),
        );
        line(
            "Sewage",
            network.sewage,
            network.utility_totals.map(|totals| {
                format!(
                    "{:.1} capacity / {:.1} demand",
                    totals.sewage_capacity, totals.sewage_demand
                )
            }),
        );
    });
}

pub(crate) fn section(sim: &Simulation, building: BuildingID) {
    let road = {
        let map = sim.map();
        let Some(building) = map.get(building) else {
            return;
        };
        building
            .connected_road
            .filter(|&road| !building.is_orphaned() && map.get(road).is_some())
    };
    if let Some(road) = road {
        render(sim, road);
    } else {
        mincolumn(3.0, || {
            heading();
            textc(
                on_secondary_container(),
                "Disconnected from the road network",
            );
        });
    }
}

fn heading() {
    tooltip(UTILITY_EXPLANATION, || {
        textc(on_secondary_container(), "Road utilities")
    });
}

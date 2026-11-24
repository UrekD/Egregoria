use goryak::{mincolumn, on_secondary_container, textc, tooltip};
use simulation::map::{water_provider_site, BuildingID, RoadID, WaterKind, WaterSite};
use simulation::map_dynamic::{utility_network_at_road, UtilityFlow, UtilityNetworkState};

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
        if network.network.is_some() && sim.read::<UtilityFlow>().water_pending(&sim.map()) {
            textc(
                on_secondary_container(),
                "Water and sewage totals update on the next utility pass",
            );
        }
    });
}

pub(crate) fn section(sim: &Simulation, building: BuildingID) {
    let (road, site) = {
        let map = sim.map();
        let Some(building) = map.get(building) else {
            return;
        };
        let road = building
            .connected_road
            .filter(|&road| map.get(road).is_some());
        (road, water_provider_site(&map, building.id))
    };
    if let Some(site) = site {
        let line = match site {
            Ok(WaterSite::Legacy { .. }) => "Site valid · legacy dry siting".to_owned(),
            Ok(WaterSite::Shore {
                contact,
                distance_m,
                ..
            }) => {
                let shore = match contact.kind {
                    WaterKind::River => "river bank",
                    WaterKind::Lake => "lake shore",
                    WaterKind::Sea => "coast",
                };
                format!("Site valid · {shore} {distance_m:.1} m away")
            }
            Err(reason) => format!("Site invalid · {reason}"),
        };
        textc(on_secondary_container(), line);
    }
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

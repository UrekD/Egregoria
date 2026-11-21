use std::collections::BTreeMap;

use prototypes::{try_prototype, ServiceID, ServiceKind};
use serde::{Deserialize, Serialize};

use crate::map::{BuildingID, BuildingKind, ElectricityNetworkID};
use crate::souls::residence::Residences;
use crate::{Simulation, SoulID};

use super::{BuildingInfos, ElectricityFlow};

const UTILITY_INTERVAL_TICKS: u64 = 3_000;

#[derive(Default, Serialize, Deserialize)]
pub struct UtilityFlow {
    // The spine saved two empty collections, which decode as two empty maps.
    by_building: BTreeMap<BuildingID, ElectricityNetworkID>,
    by_network: BTreeMap<ElectricityNetworkID, UtilityTotals>,
}

impl UtilityFlow {
    pub fn has_water(&self, building: BuildingID) -> bool {
        self.totals_for(building)
            .is_some_and(UtilityTotals::has_water)
    }

    pub fn has_sewage(&self, building: BuildingID) -> bool {
        self.totals_for(building)
            .is_some_and(UtilityTotals::has_sewage)
    }

    /// Totals from the latest utility pass, ordered by electricity network ID.
    pub fn networks(&self) -> impl Iterator<Item = (ElectricityNetworkID, UtilityTotals)> + '_ {
        self.by_network.iter().map(|(&id, &totals)| (id, totals))
    }

    fn totals_for(&self, building: BuildingID) -> Option<&UtilityTotals> {
        self.by_network.get(self.by_building.get(&building)?)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UtilityTotals {
    pub water_production: f64,
    pub water_demand: f64,
    pub sewage_capacity: f64,
    /// Water demand served by the network.
    pub sewage_demand: f64,
}

impl UtilityTotals {
    fn has_water(&self) -> bool {
        self.water_production > 0.0 && self.water_production >= self.water_demand
    }

    fn has_sewage(&self) -> bool {
        self.sewage_capacity > 0.0 && self.sewage_capacity >= self.sewage_demand
    }
}

pub fn utility_flow_update(sim: &mut Simulation) {
    if sim.get_tick() % UTILITY_INTERVAL_TICKS != 0 {
        return;
    }
    profiling::scope!("map_dynamic::utility_flow");

    let map = sim.map();
    let electricity = sim.read::<ElectricityFlow>();
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let mut flow = UtilityFlow::default();

    // Every road in the electricity graph carries pipes, including rail-only roads.
    for network in map.electricity.networks() {
        let mut totals = UtilityTotals::default();
        let blackout = electricity.blackout(network.id);
        for &id in &network.buildings {
            let Some(building) = map.get(id).filter(|building| !building.is_orphaned()) else {
                continue;
            };
            flow.by_building.insert(id, network.id);

            match building.kind {
                BuildingKind::House => {
                    let prototype = residences
                        .by_building
                        .get(&id)
                        .and_then(|residence| try_prototype(residence.prototype));
                    if let Some(proto) = prototype {
                        totals.water_demand += f64::from(proto.water_consumption.unwrap_or(0.0));
                    }
                }
                BuildingKind::GoodsCompany(company) => {
                    let Some(proto) = try_prototype(company) else {
                        continue;
                    };
                    totals.water_demand += f64::from(proto.water_consumption.unwrap_or(0.0));

                    let Some(service) = try_prototype(ServiceID::from(company)) else {
                        continue;
                    };
                    let (rate, total) = match service.service_kind {
                        ServiceKind::WaterPump if !blackout => {
                            (proto.water_production, &mut totals.water_production)
                        }
                        ServiceKind::SewageOutlet => {
                            (proto.sewage_capacity, &mut totals.sewage_capacity)
                        }
                        _ => continue,
                    };
                    let Some(SoulID::GoodsCompany(owner)) = infos.owner(id) else {
                        continue;
                    };
                    if let Some(company) = sim.world.companies.get(owner) {
                        let productivity = company.raw_productivity(proto, building.zone.as_ref());
                        *total += f64::from(rate.unwrap_or(0.0)) * f64::from(productivity);
                    }
                }
                _ => {}
            }
        }
        if totals.has_water() {
            totals.sewage_demand = totals.water_demand;
        }
        flow.by_network.insert(network.id, totals);
    }

    *sim.write::<UtilityFlow>() = flow;
}

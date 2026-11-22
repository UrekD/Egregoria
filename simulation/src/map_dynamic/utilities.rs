use std::collections::BTreeMap;

use prototypes::{try_prototype, ServiceID, ServiceKind, Tick};
use serde::{Deserialize, Serialize};

use crate::economy::balance::{periodic_tick, CITY_UPDATE_INTERVAL_TICKS};
use crate::economy::Budget;
use crate::map::{BuildingID, BuildingKind, ElectricityNetworkID, Map, RoadID, MAX_ZONE_AREA};
use crate::souls::residence::Residences;
use crate::{Simulation, SoulID};

use super::{BuildingInfos, ElectricityFlow, NetworkFlow};

const RELATIVE_RATE_EPSILON: f64 = 1e-6;

#[derive(Default, Serialize, Deserialize)]
pub struct UtilityFlow {
    by_building: BTreeMap<BuildingID, ElectricityNetworkID>,
    by_network: BTreeMap<ElectricityNetworkID, UtilityTotals>,
    #[serde(skip)]
    topology_generation: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UtilityStatus {
    Served,
    Shortage,
    Disconnected,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuildingUtilityStatus {
    pub water: UtilityStatus,
    pub sewage: UtilityStatus,
    pub totals: Option<UtilityTotals>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UtilityNetworkState {
    Updating,
    Disconnected,
    Shortage,
    Ok,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UtilityNetworkReadout {
    pub network: Option<ElectricityNetworkID>,
    pub electricity: UtilityNetworkState,
    pub water: UtilityNetworkState,
    pub sewage: UtilityNetworkState,
    pub power_totals: Option<NetworkFlow>,
    pub utility_totals: Option<UtilityTotals>,
}

pub fn utility_network_at_road(_sim: &Simulation, _road: RoadID) -> Option<UtilityNetworkReadout> {
    None
}

impl UtilityFlow {
    pub(crate) const UPDATE_OFFSET: u64 = 1;

    pub(crate) const fn due(tick: u64) -> bool {
        tick >= CITY_UPDATE_INTERVAL_TICKS
            && tick % CITY_UPDATE_INTERVAL_TICKS == Self::UPDATE_OFFSET
    }

    pub fn has_water(&self, building: BuildingID) -> bool {
        self.totals_for(building)
            .is_some_and(UtilityTotals::has_water)
    }

    pub fn has_sewage(&self, building: BuildingID) -> bool {
        self.totals_for(building)
            .is_some_and(UtilityTotals::has_sewage)
    }

    /// Returns the latest service status for a consumer. Live disconnection takes
    /// precedence over the utility snapshot, which refreshes every 3,000 ticks.
    pub fn status(
        &self,
        map: &Map,
        residences: &Residences,
        id: BuildingID,
    ) -> Option<BuildingUtilityStatus> {
        use UtilityStatus::*;

        let building = map.get(id)?;
        let consumption = match building.kind {
            BuildingKind::House => residences
                .by_building
                .get(&id)
                .and_then(|residence| try_prototype(residence.prototype))
                .and_then(|proto| proto.water_consumption),
            BuildingKind::GoodsCompany(company) => {
                try_prototype(company).and_then(|proto| proto.water_consumption)
            }
            _ => None,
        };
        if valid_rate(consumption) == 0.0 {
            return None;
        }

        let unavailable = |status| BuildingUtilityStatus {
            water: status,
            sewage: status,
            totals: None,
        };
        let Some(network) = map
            .electricity
            .net_id(id)
            .filter(|_| !building.is_orphaned())
        else {
            return Some(unavailable(Disconnected));
        };
        let Some(&totals) = self
            .by_building
            .get(&id)
            .filter(|&&observed| observed == network)
            .and_then(|observed| self.by_network.get(observed))
        else {
            return Some(unavailable(Unknown));
        };
        Some(BuildingUtilityStatus {
            water: if totals.has_water() { Served } else { Shortage },
            sewage: if totals.has_sewage() {
                Served
            } else {
                Shortage
            },
            totals: Some(totals),
        })
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
    pub fn has_water(&self) -> bool {
        meets_demand(self.water_production, self.water_demand)
    }

    pub fn has_sewage(&self) -> bool {
        meets_demand(self.sewage_capacity, self.sewage_demand)
    }
}

fn meets_demand(supply: f64, demand: f64) -> bool {
    supply > 0.0 && demand - supply <= demand * RELATIVE_RATE_EPSILON
}

fn valid_rate(rate: Option<f32>) -> f64 {
    rate.filter(|rate| rate.is_finite() && *rate >= 0.0)
        .map_or(0.0, f64::from)
}

pub fn utility_flow_update(sim: &mut Simulation) {
    let tick = periodic_tick(&sim.read::<Budget>(), Tick(sim.get_tick()));
    if !UtilityFlow::due(tick) {
        return;
    }
    profiling::scope!("map_dynamic::utility_flow");

    let map = sim.map();
    let electricity = sim.read::<ElectricityFlow>();
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let mut flow = sim.write::<UtilityFlow>();
    let generation = map.electricity.generation();
    let topology_changed = flow.topology_generation != Some(generation);
    if topology_changed {
        // Keep existing entries where possible, including on the first pass after loading.
        flow.by_building.retain(|&id, _| {
            map.get(id).is_some_and(|building| !building.is_orphaned())
                && map.electricity.net_id(id).is_some()
        });
        flow.by_network
            .retain(|id, _| map.electricity.networks.contains_key(id));
        flow.topology_generation = Some(generation);
    }

    // Every road in the electricity graph carries pipes, including rail-only roads.
    for network in map.electricity.networks() {
        let mut totals = UtilityTotals::default();
        let blackout = electricity.blackout(network.id);
        for &id in &network.buildings {
            let Some(building) = map.get(id).filter(|building| !building.is_orphaned()) else {
                continue;
            };
            if topology_changed {
                flow.by_building.insert(id, network.id);
            }

            match building.kind {
                BuildingKind::House => {
                    let prototype = residences
                        .by_building
                        .get(&id)
                        .and_then(|residence| try_prototype(residence.prototype));
                    if let Some(proto) = prototype {
                        totals.water_demand += valid_rate(proto.water_consumption);
                    }
                }
                BuildingKind::GoodsCompany(company) => {
                    let Some(proto) = try_prototype(company) else {
                        continue;
                    };
                    totals.water_demand += valid_rate(proto.water_consumption);

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
                        let staffing = company.workers.0.len() as f64 / f64::from(proto.n_workers);
                        let zone_factor = building
                            .zone
                            .as_ref()
                            .map_or(1.0, |zone| f64::from(zone.area) / f64::from(MAX_ZONE_AREA));
                        let effective_rate = valid_rate(rate) * staffing * zone_factor;
                        if effective_rate.is_finite() && effective_rate >= 0.0 {
                            *total += effective_rate;
                        }
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
}

use std::collections::{BTreeMap, BTreeSet};

use prototypes::{try_prototype, Power, ServiceID, ServiceKind, Tick};
use serde::{Deserialize, Serialize};

use crate::economy::balance::{periodic_tick, CITY_UPDATE_INTERVAL_TICKS};
use crate::economy::Budget;
use crate::map::{
    BuildingID, BuildingKind, ElectricityNetworkID, Map, NetworkObjectID, RoadID, MAX_ZONE_AREA,
};
use crate::souls::residence::Residences;
use crate::{Simulation, SoulID};

use super::{BuildingInfos, ElectricityFlow, NetworkFlow};

const RELATIVE_RATE_EPSILON: f64 = 1e-6;

#[derive(Default, Serialize, Deserialize)]
pub struct UtilityFlow {
    by_building: BTreeMap<BuildingID, ElectricityNetworkID>,
    by_network: BTreeMap<ElectricityNetworkID, UtilityTotals>,
    #[serde(skip)]
    sampled_generation: Option<u64>,
    #[serde(skip)]
    pub(crate) sampled_members: BTreeMap<ElectricityNetworkID, BTreeSet<NetworkObjectID>>,
    #[serde(skip)]
    pub(crate) sampled_water_revision: u64,
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
    Idle,
    Shortage,
    Ok,
}

impl UtilityNetworkState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Updating => "Updating",
            Self::Disconnected => "Disconnected",
            Self::Idle => "Idle",
            Self::Shortage => "Shortage",
            Self::Ok => "OK",
        }
    }
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

/// A missing road has no readout. A live road outside the utility graph is disconnected.
pub fn utility_network_at_road(sim: &Simulation, road: RoadID) -> Option<UtilityNetworkReadout> {
    let map = sim.map();
    map.get(road)?;
    Some(utility_network_readout(
        &map,
        &sim.read::<ElectricityFlow>(),
        &sim.read::<UtilityFlow>(),
        map.electricity.net_id(road),
    ))
}

/// Read the latest power flow and water/sewage totals for the current component.
pub fn utility_network_readout(
    map: &Map,
    power: &ElectricityFlow,
    flow: &UtilityFlow,
    network: Option<ElectricityNetworkID>,
) -> UtilityNetworkReadout {
    use UtilityNetworkState::{Disconnected, Idle, Ok, Shortage, Updating};

    let network = network.filter(|id| map.electricity.networks.contains_key(id));
    let Some(id) = network else {
        return UtilityNetworkReadout {
            network,
            electricity: Disconnected,
            water: Disconnected,
            sewage: Disconnected,
            power_totals: None,
            utility_totals: None,
        };
    };

    let power_totals = power.network_stats(id);
    let electricity = if power_totals.produced_power == Power::ZERO
        && power_totals.consumed_power == Power::ZERO
    {
        Idle
    } else if power_totals.blackout || power_totals.produced_power <= Power::ZERO {
        Shortage
    } else {
        Ok
    };

    let water_pending = flow.water_pending(map);
    let utility_state = |supply, demand| {
        if water_pending {
            Updating
        } else if supply == 0.0 && demand == 0.0 {
            Idle
        } else if meets_demand(supply, demand) {
            Ok
        } else {
            Shortage
        }
    };
    let utility_totals = flow.current_totals(map, id);
    UtilityNetworkReadout {
        network,
        electricity,
        water: utility_totals.map_or(Updating, |totals| {
            utility_state(totals.water_production, totals.water_demand)
        }),
        sewage: utility_totals.map_or(Updating, |totals| {
            utility_state(totals.sewage_capacity, totals.sewage_demand)
        }),
        power_totals: Some(power_totals),
        utility_totals,
    }
}

pub fn city_field_tick(sim: &Simulation) -> u64 {
    periodic_tick(&sim.read::<Budget>(), Tick(sim.get_tick()))
}

impl UtilityFlow {
    pub fn water_pending(&self, map: &Map) -> bool {
        self.sampled_water_revision != map.environment().water_revision()
    }

    pub(crate) const UPDATE_OFFSET: u64 = 1;

    pub fn topology_revision(map: &Map) -> u64 {
        map.electricity.generation()
    }

    fn current_totals(&self, map: &Map, id: ElectricityNetworkID) -> Option<UtilityTotals> {
        // Skip the membership comparison until the graph changes.
        if self.sampled_generation != Some(map.electricity.generation())
            && self.sampled_members.get(&id)? != &map.electricity.networks.get(&id)?.objects
        {
            return None;
        }
        self.by_network.get(&id).copied()
    }

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
    let tick = city_field_tick(sim);
    if !UtilityFlow::due(tick) {
        return;
    }
    profiling::scope!("map_dynamic::utility_flow");

    let map = sim.map();
    let electricity = sim.read::<ElectricityFlow>();
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let mut flow = sim.write::<UtilityFlow>();
    flow.sampled_water_revision = map.environment().water_revision();
    let generation = map.electricity.generation();
    let topology_changed = flow.sampled_generation != Some(generation);
    if topology_changed {
        // Keep existing entries where possible, including on the first pass after loading.
        flow.by_building.retain(|&id, _| {
            map.get(id).is_some_and(|building| !building.is_orphaned())
                && map.electricity.net_id(id).is_some()
        });
        flow.by_network
            .retain(|id, _| map.electricity.networks.contains_key(id));
        flow.sampled_members = map
            .electricity
            .networks()
            .map(|network| (network.id, network.objects.clone()))
            .collect();
        flow.sampled_generation = Some(generation);
    }

    let water_sites = map
        .environment()
        .strict_water_siting()
        .then(|| crate::map::river_site_summary(&map));

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
                    if water_sites.as_ref().is_some_and(|sites| {
                        !sites.provider_site(id).is_some_and(|site| site.is_ok())
                    }) {
                        continue;
                    }
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

#[cfg(test)]
mod tests {
    use geom::{vec2, vec3, Vec2, OBB};

    use super::*;
    use crate::map_dynamic::electricity_flow_system;
    use crate::tests::TestCtx;
    use prototypes::{BuildingGen, GameTime};

    #[test]
    fn u3_utility_readout_updates_across_connect_and_sever() {
        use UtilityNetworkState::*;

        let mut test = TestCtx::new();
        let a_end = vec3(440.0, 100.0, 0.0);
        let b_end = vec3(440.0, 300.0, 0.0);
        let initial: Vec<_> = test.g.map().roads().keys().collect();
        test.build_roads(&[vec3(80.0, 100.0, 0.0), a_end]);
        test.build_roads(&[vec3(80.0, 300.0, 0.0), b_end]);
        let roads: Vec<_> = test.g.map().roads().keys().collect();
        let new: Vec<_> = roads
            .iter()
            .copied()
            .filter(|id| !initial.contains(id))
            .collect();
        let [a, b] = new[..] else {
            panic!("two isolated roads expected")
        };
        let house = test
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(vec2(140.0, 125.0), Vec2::X, 20.0, 20.0),
                BuildingKind::House,
                BuildingGen::House,
                None,
                Some(a),
            )
            .unwrap();
        test.g.write::<BuildingInfos>().insert(house);
        crate::souls::residence::on_house_built(&test.g, house);

        let read = |sim: &Simulation, road| utility_network_at_road(sim, road).expect("live road");
        assert_eq!(read(&test.g, a).water, Updating);
        assert_eq!(utility_network_at_road(&test.g, RoadID::default()), None);

        let refresh = |sim: &mut Simulation, tick| {
            sim.write::<GameTime>().tick = Tick(tick);
            let (world, resources) = sim.world_res();
            electricity_flow_system(world, resources);
            utility_flow_update(sim);
        };
        refresh(&mut test.g, 3001);
        assert_ne!(read(&test.g, a).network, read(&test.g, b).network);
        assert_eq!(read(&test.g, a).electricity, Shortage);
        assert_eq!(read(&test.g, a).water, Shortage);
        assert_eq!(read(&test.g, b).electricity, Idle);
        assert_eq!(read(&test.g, b).water, Idle);
        assert_eq!(read(&test.g, b).sewage, Idle);
        assert!(read(&test.g, a).utility_totals.unwrap().water_demand > 0.0);
        assert_eq!(read(&test.g, b).utility_totals.unwrap().water_demand, 0.0);

        // Seed the latest supply totals so this fixture can isolate topology behavior.
        let a_net = read(&test.g, a).network.unwrap();
        {
            let mut flow = test.g.write::<UtilityFlow>();
            let totals = flow.by_network.get_mut(&a_net).unwrap();
            totals.water_production = totals.water_demand;
            totals.sewage_capacity = 500.0;
        }
        assert_eq!(read(&test.g, a).water, Ok);
        assert_eq!(read(&test.g, a).sewage, Ok);
        let previous = read(&test.g, a);
        test.build_roads(&[vec3(80.0, 500.0, 0.0), vec3(440.0, 500.0, 0.0)]);
        assert_eq!(read(&test.g, a), previous);
        let roads: Vec<_> = test.g.map().roads().keys().collect();

        test.build_roads(&[a_end, b_end]);
        let bridge = test
            .g
            .map()
            .roads()
            .keys()
            .find(|road| !roads.contains(road))
            .unwrap();
        assert_eq!(read(&test.g, a).network, read(&test.g, b).network);
        assert_eq!(read(&test.g, a).water, Updating);
        assert_eq!(read(&test.g, b).utility_totals, None);
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        assert_eq!(read(&test.g, a).electricity, Shortage);
        assert!(read(&test.g, a).power_totals.is_some());
        assert_eq!(read(&test.g, a).water, Updating);
        assert_eq!(read(&test.g, a).sewage, Updating);
        refresh(&mut test.g, 6001);
        assert_eq!(read(&test.g, a), read(&test.g, b));

        test.g.map_mut().remove_road(bridge).unwrap();
        assert_eq!(read(&test.g, a).water, Updating);
        assert_eq!(read(&test.g, b).water, Updating);
        refresh(&mut test.g, 9001);
        assert_ne!(read(&test.g, a).network, read(&test.g, b).network);
        assert_eq!(read(&test.g, b).utility_totals.unwrap().water_demand, 0.0);

        test.g.map_mut().electricity.remove_object(b);
        assert_eq!(read(&test.g, b).water, Disconnected);
    }
}

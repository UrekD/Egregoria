use std::collections::{btree_map::Entry, BTreeMap};

use geom::Vec2;
use prototypes::MAX_PROFILED_SLOTS;

use crate::economy::balance::RETAIL_ACCESS_RADIUS_M;
use crate::map::{river_site_summary, Building, BuildingID, ElectricityNetworkID, Map};
use crate::map_dynamic::{
    active_business, effective_load, utility_network_readout, ActiveBusiness, BuildingInfos,
    BuildingUtilityStatus, BusinessRates, ElectricityFlow, SiteLoad, UtilityFlow, UtilityStatus,
    UtilityTotals,
};
use crate::souls::residence::{self, PowerStatus, Residence, Residences};
use crate::{Simulation, World};

impl ActiveBusiness {
    /// Checked market-quantity change for a profiled firm upgrade. A firm that
    /// already reached its slot cap can advance with zero new openings.
    pub fn job_slot_increase(self, next_slots: u32) -> Option<i32> {
        if !(1..=MAX_PROFILED_SLOTS).contains(&next_slots) {
            return None;
        }
        i32::try_from(next_slots.checked_sub(self.slots)?).ok()
    }

    /// Preserve the legacy nominal-worker ratio for demand. Profiled premises
    /// derive staffing and cadence from the committed role.
    pub(crate) fn demand_rates(self, building: &Building) -> BusinessRates {
        let mut rates = self.rates();
        if building.development().is_none() {
            rates.staffed_fraction = if self.prototype.n_workers == 0 {
                1.0
            } else {
                f64::from(self.filled as f32 / self.prototype.n_workers as f32)
            };
        }
        rates
    }
}

/// A live frontage and graph node are both needed to connect a profiled site.
pub fn connected_site_network(map: &Map, building: &Building) -> Option<ElectricityNetworkID> {
    let road = building.connected_road?;
    map.get(road)?;
    let network = map.electricity.net_id(road)?;
    (map.electricity.net_id(building.id) == Some(network)).then_some(network)
}

/// Read connection state without treating an isolated profiled building node as served.
pub fn site_power_status(map: &Map, flow: &ElectricityFlow, building: &Building) -> PowerStatus {
    if building.development().is_some() && connected_site_network(map, building).is_none() {
        return PowerStatus::MissingNetwork;
    }
    residence::power_status(map, flow, building.id)
}

/// Forecast consumption with the proposed housing and all proposed jobs filled.
pub fn projected_site_load(
    building: &Building,
    home: Option<&Residence>,
    business: Option<ActiveBusiness>,
) -> SiteLoad {
    effective_load(
        building,
        home,
        business.map(|business| ActiveBusiness {
            filled: business.slots as usize,
            ..business
        }),
    )
}

impl SiteLoad {
    pub fn power_demand_watts(self) -> i128 {
        i128::from(self.residential_power.watts()) + i128::from(self.business_power.watts())
    }
}

/// Live network inputs for utility and staffing forecasts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetworkLoad {
    pub power_supply_watts: i128,
    pub power_demand_watts: i128,
    pub pipes: UtilityTotals,
}

impl NetworkLoad {
    pub fn live(
        map: &Map,
        residences: &Residences,
        infos: &BuildingInfos,
        world: &World,
        network: ElectricityNetworkID,
    ) -> Option<Self> {
        let mut total = Self::rated(map, residences, infos, world, network)?;
        // Leave potential provider capacity available for forecasts that add power.
        total.update_sewage_demand();
        Some(total)
    }

    fn rated(
        map: &Map,
        residences: &Residences,
        infos: &BuildingInfos,
        world: &World,
        network: ElectricityNetworkID,
    ) -> Option<Self> {
        let network = map.electricity.networks.get(&network)?;
        let water_sites = map
            .environment()
            .strict_water_siting()
            .then(|| river_site_summary(map));
        let mut total = Self::default();

        for &id in &network.buildings {
            let Some(building) = map.get(id) else {
                continue;
            };
            let connected = connected_site_network(map, building).is_some();
            let load = effective_load(
                building,
                residences.by_building.get(&id),
                active_business(building, infos, world),
            );
            if connected || building.development().is_none() {
                total.power_demand_watts += load.power_demand_watts();
                total.power_supply_watts += i128::from(load.power_production.watts());
            }
            if connected {
                total.pipes.water_demand += load.water_demand();
                total.pipes.sewage_demand += load.sewage_demand();
                if (load.water_production > 0.0 || load.sewage_capacity > 0.0)
                    && water_sites.as_ref().is_none_or(|sites| {
                        sites.provider_site(id).is_some_and(|site| site.is_ok())
                    })
                {
                    total.pipes.water_production += load.water_production;
                    total.pipes.sewage_capacity += load.sewage_capacity;
                }
            }
        }
        Some(total)
    }

    fn cap_pipe_supply(&mut self, sampled: UtilityTotals) {
        self.pipes.water_production = self.pipes.water_production.min(sampled.water_production);
        self.pipes.sewage_capacity = self.pipes.sewage_capacity.min(sampled.sewage_capacity);
        if self.blackout() {
            self.pipes.water_production = 0.0;
        }
        self.update_sewage_demand();
    }

    fn update_sewage_demand(&mut self) {
        if self.blackout() || !self.pipes.has_water() {
            self.pipes.sewage_demand = 0.0;
        }
    }

    fn blackout(self) -> bool {
        self.power_demand_watts > self.power_supply_watts
    }

    pub fn has_power(self) -> bool {
        self.power_supply_watts > 0 && !self.blackout()
    }

    fn reserve(&mut self, current: SiteLoad, projected: SiteLoad) -> Result<(), UtilityAdmission> {
        let mut next = *self;
        let power = projected.power_demand_watts();
        let water = projected.water_demand();
        let sewage = projected.sewage_demand();
        next.power_demand_watts += (power - current.power_demand_watts()).max(0);
        next.pipes.water_demand += (water - current.water_demand()).max(0.0);
        next.pipes.sewage_demand += (sewage - current.sewage_demand()).max(0.0);

        if power > 0 && !next.has_power() {
            return Err(UtilityAdmission::Power);
        }
        if water > 0.0 && (next.blackout() || !next.pipes.has_water()) {
            return Err(UtilityAdmission::Water);
        }
        if sewage > 0.0 && !next.pipes.has_sewage() {
            return Err(UtilityAdmission::Sewage);
        }
        *self = next;
        Ok(())
    }
}

fn sampled_pipes(
    map: &Map,
    power: &ElectricityFlow,
    flow: &UtilityFlow,
    network: ElectricityNetworkID,
) -> Option<UtilityTotals> {
    if flow.water_pending(map) {
        return None;
    }
    utility_network_readout(map, power, flow, Some(network)).utility_totals
}

/// Read service using current network roles and the latest valid provider sample.
pub(crate) fn live_site_utility_status(
    map: &Map,
    residences: &Residences,
    infos: &BuildingInfos,
    world: &World,
    power: &ElectricityFlow,
    flow: &UtilityFlow,
    id: BuildingID,
) -> Option<BuildingUtilityStatus> {
    use UtilityStatus::*;

    let building = map.get(id)?;
    let load = effective_load(
        building,
        residences.by_building.get(&id),
        active_business(building, infos, world),
    );
    if load.water_demand() == 0.0 && load.sewage_demand() == 0.0 {
        return None;
    }

    let unavailable = |status| BuildingUtilityStatus {
        water: status,
        sewage: status,
        totals: None,
    };
    let Some(network) = connected_site_network(map, building) else {
        return Some(unavailable(Disconnected));
    };
    let Some(sampled) = sampled_pipes(map, power, flow, network) else {
        return Some(unavailable(Unknown));
    };
    let mut totals = NetworkLoad::rated(map, residences, infos, world, network)?;
    totals.cap_pipe_supply(sampled);
    let pipes = totals.pipes;
    Some(BuildingUtilityStatus {
        water: if pipes.has_water() { Served } else { Shortage },
        sewage: if pipes.has_sewage() { Served } else { Shortage },
        totals: Some(pipes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z3_fix_idle_network_delivers_water_from_a_zero_watt_pump() {
        let mut network = NetworkLoad {
            pipes: UtilityTotals {
                water_production: 10.0,
                water_demand: 4.0,
                sewage_capacity: 10.0,
                sewage_demand: 4.0,
            },
            ..Default::default()
        };
        assert!(!network.has_power());
        network.update_sewage_demand();
        assert_eq!(network.pipes.sewage_demand, 4.0);
        network.power_demand_watts = 1;
        network.update_sewage_demand();
        assert_eq!(network.pipes.sewage_demand, 0.0);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UtilityAdmission {
    Disconnected,
    StaleTopology,
    Power,
    Water,
    Sewage,
}

/// One ledger per upgrade pass. Callers admit candidates in BuildingID order
/// after closing unavailable sites, and record only upgrades that will commit.
pub struct NetworkReservations {
    topology: (u64, u64, u64),
    networks: BTreeMap<ElectricityNetworkID, NetworkLoad>,
}

impl NetworkReservations {
    pub fn new(map: &Map) -> Self {
        Self {
            topology: Self::topology(map),
            networks: BTreeMap::new(),
        }
    }

    fn topology(map: &Map) -> (u64, u64, u64) {
        (
            map.identity(),
            map.electricity.generation(),
            map.environment().water_revision(),
        )
    }

    pub fn try_reserve(
        &mut self,
        sim: &Simulation,
        id: BuildingID,
        projected: SiteLoad,
    ) -> Result<(), UtilityAdmission> {
        let map = sim.map();
        if Self::topology(&map) != self.topology {
            return Err(UtilityAdmission::StaleTopology);
        }
        let building = map.get(id).ok_or(UtilityAdmission::Disconnected)?;
        let network =
            connected_site_network(&map, building).ok_or(UtilityAdmission::Disconnected)?;
        let residences = sim.read::<Residences>();
        let infos = sim.read::<BuildingInfos>();
        let world = sim.world();
        let current = effective_load(
            building,
            residences.by_building.get(&id),
            active_business(building, &infos, world),
        );
        let load = match self.networks.entry(network) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let power = sim.read::<ElectricityFlow>();
                let water = sim.read::<UtilityFlow>();
                let mut load = NetworkLoad::rated(&map, &residences, &infos, world, network)
                    .ok_or(UtilityAdmission::Disconnected)?;
                load.cap_pipe_supply(
                    sampled_pipes(&map, &power, &water, network).unwrap_or_default(),
                );
                entry.insert(load)
            }
        };
        load.reserve(current, projected)
    }
}

/// Spatial lookup for staffed, open retail. Build once per read pass so a
/// fresh pass sees first hires and closures without waiting for a map edit.
#[derive(Default)]
pub struct ActiveRetailIndex {
    cells: BTreeMap<(i32, i32), Vec<Vec2>>,
}

impl ActiveRetailIndex {
    pub fn build(map: &Map, infos: &BuildingInfos, world: &World) -> Self {
        let mut index = Self::default();
        for (id, entity) in world.companies.iter() {
            if entity.workers.0.is_empty() {
                continue;
            }
            let Some(building) = map.get(entity.comp.building) else {
                continue;
            };
            if residence::is_retail_store(building.kind)
                && building
                    .connected_road
                    .is_some_and(|road| map.get(road).is_some())
                && active_business(building, infos, world)
                    .is_some_and(|business| business.company == id)
            {
                let door = building.door_pos.xy();
                index.cells.entry(Self::cell(door)).or_default().push(door);
            }
        }
        index
    }

    fn cell(door: Vec2) -> (i32, i32) {
        let cell = door / RETAIL_ACCESS_RADIUS_M;
        (cell.x.floor() as i32, cell.y.floor() as i32)
    }

    pub fn has_access(&self, door: Vec2) -> bool {
        self.nearby(door).next().is_some()
    }

    pub(crate) fn nearby(&self, door: Vec2) -> impl Iterator<Item = Vec2> + '_ {
        let (x, y) = Self::cell(door);
        (x.saturating_sub(1)..=x.saturating_add(1))
            .flat_map(move |x| (y.saturating_sub(1)..=y.saturating_add(1)).map(move |y| (x, y)))
            .filter_map(|cell| self.cells.get(&cell))
            .flatten()
            .copied()
            .filter(move |store| store.distance2(door) <= RETAIL_ACCESS_RADIUS_M.powi(2))
    }
}

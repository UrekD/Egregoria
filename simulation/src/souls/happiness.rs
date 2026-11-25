use std::collections::BTreeMap;

use prototypes::{Tick, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};

use crate::economy::{
    balance::{
        periodic_tick, HAPPINESS_CLEAN_AIR, HAPPINESS_COMMUTE, HAPPINESS_COMMUTE_LIMIT_M,
        HAPPINESS_COVERAGE, HAPPINESS_EMPLOYMENT, HAPPINESS_LAND_VALUE, HAPPINESS_POWER,
        HAPPINESS_RETAIL, HAPPINESS_SEWAGE, HAPPINESS_TAX, HAPPINESS_WATER, NEUTRAL_HAPPINESS,
    },
    Budget, MAX_TAX_RATE_PERMILLE,
};
use crate::map::{BuildingID, ZoneKind};
use crate::map_dynamic::{
    site_power_status, ActiveRetailIndex, BuildingInfos, CoverageGrid, ElectricityFlow, EnvGrid,
};
use crate::{Simulation, SoulID};

use super::development::network_services;
use super::residence::{PowerStatus, Residences};

pub(crate) const UPDATE_OFFSET: u64 = 3;

#[derive(Default, Serialize, Deserialize)]
pub struct Happiness {
    pub by_building: BTreeMap<BuildingID, u8>,
}

impl Happiness {
    pub fn score(&self, building: BuildingID) -> Option<u8> {
        self.by_building.get(&building).copied()
    }

    pub fn average(&self, infos: &BuildingInfos) -> f32 {
        let mut residents = 0;
        let mut total = 0;
        for (&building, &score) in &self.by_building {
            if let Some(info) = infos.get(building) {
                let occupied = info.residents.len() as u64;
                residents += occupied;
                total += occupied * u64::from(score);
            }
        }
        if residents == 0 {
            NEUTRAL_HAPPINESS
        } else {
            total as f32 / residents as f32
        }
    }
}

pub(crate) const fn due(tick: u64) -> bool {
    tick >= TICKS_PER_HOUR && tick % TICKS_PER_HOUR == UPDATE_OFFSET
}

pub fn happiness_update(sim: &mut Simulation) {
    // Follow the producer's elapsed-time phase even when the calendar changes.
    if !due(periodic_tick(&sim.read::<Budget>(), Tick(sim.get_tick()))) {
        return;
    }
    profiling::scope!("souls::happiness_update");

    let map = sim.map();
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let electricity = sim.read::<ElectricityFlow>();
    let coverage = sim.read::<CoverageGrid>();
    let environment = sim.read::<EnvGrid>();
    let stores = ActiveRetailIndex::build(&map, &infos, sim.world());
    let tax_rate = sim.read::<Budget>().tax_rate(ZoneKind::Residential);
    let tax_score = 1.0 - f32::from(tax_rate) / f32::from(MAX_TAX_RATE_PERMILLE);
    let mut scores = BTreeMap::new();
    let mut services = BTreeMap::new();

    for &id in residences.by_building.keys() {
        let Some(home) = map.get(id).filter(|home| !home.is_abandoned_growable()) else {
            continue;
        };
        let door = home.door_pos.xy();
        let mut occupied = 0;
        let mut employed = 0;
        let mut commute_score = 0.0;
        for &resident in infos.get(id).into_iter().flat_map(|info| &info.residents) {
            let SoulID::Human(human) = resident else {
                continue;
            };
            let Some(human) = sim.get(human) else {
                continue;
            };
            occupied += 1;
            if let Some(workplace) = human.work.as_ref().and_then(|work| map.get(work.workplace)) {
                employed += 1;
                let distance = door.distance(workplace.door_pos.xy());
                commute_score += (1.0 - distance / HAPPINESS_COMMUTE_LIMIT_M).max(0.0);
            }
        }

        let service_score: f32 = HAPPINESS_COVERAGE
            .iter()
            .map(|&(kind, weight)| coverage.coverage(id, kind) / 100.0 * weight)
            .sum();
        let pollution = environment.pollution_at(home.obb.center());
        let utility = network_services(sim, &map, home, &mut services);
        let score = service_score
            + f32::from(site_power_status(&map, &electricity, home) == PowerStatus::Available)
                * HAPPINESS_POWER
            + f32::from(utility.water) * HAPPINESS_WATER
            + f32::from(utility.sewage) * HAPPINESS_SEWAGE
            + (1.0 - pollution / 100.0).max(0.0) * HAPPINESS_CLEAN_AIR
            + f32::from(environment.land_value(id)) / 100.0 * HAPPINESS_LAND_VALUE
            + tax_score * HAPPINESS_TAX
            + f32::from(stores.has_access(door)) * HAPPINESS_RETAIL
            + employed as f32 / occupied.max(1) as f32 * HAPPINESS_EMPLOYMENT
            + commute_score / employed.max(1) as f32 * HAPPINESS_COMMUTE;
        scores.insert(id, score.round().clamp(0.0, 100.0) as u8);
    }
    sim.write::<Happiness>().by_building = scores;
}

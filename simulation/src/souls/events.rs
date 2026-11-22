use std::collections::BTreeMap;

use prototypes::{ServiceKind, Tick, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use slotmapd::Key;

use crate::map::{BuildingID, BuildingKind};
use crate::map_dynamic::{CoverageGrid, EnvGrid};
use crate::utils::rand_provider::RandProvider;
use crate::{Simulation, RNG_SEED};

const EVENT_FIRE: u64 = 0x4649_5245;
const FIRE_CHANCE: f32 = 0.002;
const COLLAPSE_TICKS: u64 = 3 * TICKS_PER_HOUR;

#[derive(Default, Serialize, Deserialize)]
pub struct Fires {
    /// Tick at which each active fire started.
    pub by_building: BTreeMap<BuildingID, Tick>,
}

pub fn events_update(sim: &mut Simulation) {
    let tick = sim.get_tick();
    if tick % TICKS_PER_HOUR != 0 {
        return;
    }
    profiling::scope!("souls::events_update");

    let mut collapse = Vec::new();
    {
        let map = sim.map();
        let coverage = sim.read::<CoverageGrid>();
        let environment = sim.read::<EnvGrid>();
        let mut fires = sim.write::<Fires>();
        let hour = tick / TICKS_PER_HOUR;

        // A building extinguished this hour is eligible to ignite again next hour.
        let ignitions: Vec<_> = map
            .buildings()
            .values()
            .filter_map(|building| {
                let id = building.id;
                if !can_burn(building.kind) || fires.by_building.contains_key(&id) {
                    return None;
                }
                let pollution = environment.pollution_at(building.obb.center());
                let fire_coverage = coverage.coverage(id, ServiceKind::Fire);
                let chance = FIRE_CHANCE * (1.0 + pollution / 50.0) * (1.0 - fire_coverage / 100.0);
                (fire_draw(hour, id) < chance).then_some((id, Tick(tick)))
            })
            .collect();

        fires.by_building.retain(|&id, start| {
            if !map.get(id).is_some_and(|building| can_burn(building.kind))
                || coverage.coverage(id, ServiceKind::Fire) > 0.0
            {
                return false;
            }
            if tick.saturating_sub(start.0) >= COLLAPSE_TICKS {
                collapse.push(id);
                return false;
            }
            true
        });
        fires.by_building.extend(ignitions);
    }
    for id in collapse {
        super::growth::remove_growable(sim, id);
    }
}

fn can_burn(kind: BuildingKind) -> bool {
    kind == BuildingKind::House || kind.as_goods_company().is_some_and(|id| id.is_growable())
}

fn fire_draw(hour: u64, building: BuildingID) -> f32 {
    let seed = [RNG_SEED, hour, building.data().as_ffi(), EVENT_FIRE]
        .into_iter()
        .fold(0, |seed, field| splitmix64(seed ^ field));
    RandProvider::new(seed).next_f32()
}

fn splitmix64(value: u64) -> u64 {
    let value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
pub(crate) fn force_ignite(sim: &Simulation, building: BuildingID) {
    sim.write::<Fires>()
        .by_building
        .insert(building, Tick(sim.get_tick()));
}

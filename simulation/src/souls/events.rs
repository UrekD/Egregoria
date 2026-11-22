use std::collections::{BTreeMap, VecDeque};

use prototypes::{ServiceKind, Tick, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use slotmapd::Key;

use crate::economy::Budget;
use crate::map::{BuildingID, BuildingKind};
use crate::map_dynamic::{CoverageGrid, EnvGrid};
use crate::utils::rand_provider::RandProvider;
use crate::{Simulation, INITIAL_TICK, RNG_SEED};

const EVENT_FIRE: u64 = 0x4649_5245;
const FIRE_CHANCE: f32 = 0.002;
const COLLAPSE_TICKS: u64 = 3 * TICKS_PER_HOUR;
const EVENT_HISTORY_LIMIT: usize = 256;
pub(crate) const UPDATE_OFFSET: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FireEventKind {
    Ignited,
    Extinguished,
    Collapsed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FireEvent {
    pub seq: u64,
    /// Elapsed simulation tick.
    pub tick: Tick,
    pub building: BuildingID,
    pub kind: FireEventKind,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Fires {
    /// Elapsed tick at which each active fire started.
    pub by_building: BTreeMap<BuildingID, Tick>,
    pub events: VecDeque<FireEvent>,
    last_event_seq: u64,
}

impl Fires {
    /// Iterates over retained events newer than the given sequence, oldest first.
    pub fn events_since(&self, seq: u64) -> impl Iterator<Item = &FireEvent> {
        self.events.iter().filter(move |event| event.seq > seq)
    }

    fn record(&mut self, tick: Tick, building: BuildingID, kind: FireEventKind) {
        self.last_event_seq += 1;
        if self.events.len() == EVENT_HISTORY_LIMIT {
            self.events.pop_front();
        }
        self.events.push_back(FireEvent {
            seq: self.last_event_seq,
            tick,
            building,
            kind,
        });
    }
}

fn elapsed_tick(sim: &Simulation) -> Tick {
    Tick(sim.read::<Budget>().elapsed_ticks(Tick(sim.get_tick())))
}

pub(crate) const fn due(tick: u64) -> bool {
    tick >= TICKS_PER_HOUR && tick % TICKS_PER_HOUR == UPDATE_OFFSET
}

pub fn events_update(sim: &mut Simulation) {
    if !due(sim.get_tick()) {
        return;
    }
    profiling::scope!("souls::events_update");

    // Schedule on the shared phase, while keeping fire ages and RNG keys on
    // elapsed simulation time so calendar jumps do not directly advance them.
    let tick = elapsed_tick(sim);
    let hour = tick.0.saturating_add(INITIAL_TICK) / TICKS_PER_HOUR;
    let mut collapse = Vec::new();
    {
        let map = sim.map();
        let coverage = sim.read::<CoverageGrid>();
        let environment = sim.read::<EnvGrid>();
        let mut fires = sim.write::<Fires>();

        // A building extinguished this hour is eligible to ignite again next hour.
        let mut ignitions: Vec<_> = map
            .buildings()
            .values()
            .filter_map(|building| {
                let id = building.id;
                if !can_burn(building.kind) || fires.by_building.contains_key(&id) {
                    return None;
                }
                let pollution = environment.pollution_at(building.obb.center());
                let fire_coverage = coverage.coverage(id, ServiceKind::Fire);
                let chance = FIRE_CHANCE
                    * (1.0 + pollution.min(100.0) / 50.0)
                    * (1.0 - fire_coverage / 100.0);
                (fire_draw(hour, id) < chance).then_some(id)
            })
            .collect();
        ignitions.sort_unstable();

        let mut resolved = Vec::new();
        fires.by_building.retain(|&id, start| {
            if !map.get(id).is_some_and(|building| can_burn(building.kind)) {
                return false;
            }
            if coverage.coverage(id, ServiceKind::Fire) > 0.0 {
                resolved.push((id, FireEventKind::Extinguished));
                return false;
            }
            if tick.0.saturating_sub(start.0) >= COLLAPSE_TICKS {
                collapse.push(id);
                resolved.push((id, FireEventKind::Collapsed));
                return false;
            }
            true
        });
        for (id, kind) in resolved {
            fires.record(tick, id, kind);
        }
        for id in ignitions {
            fires.by_building.insert(id, tick);
            fires.record(tick, id, FireEventKind::Ignited);
        }
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
    let tick = elapsed_tick(sim);
    let mut fires = sim.write::<Fires>();
    fires.by_building.insert(building, tick);
    fires.record(tick, building, FireEventKind::Ignited);
}

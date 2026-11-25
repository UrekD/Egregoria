use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use prototypes::{ServiceKind, Tick, TICKS_PER_HOUR};
use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use slotmapd::Key;

use crate::economy::balance::elapsed_phase_tick;
use crate::economy::Budget;
use crate::map::{BuildingID, BuildingKind};
use crate::map_dynamic::{CoverageGrid, EnvGrid};
use crate::utils::rand_provider::RandProvider;
use crate::{Simulation, RNG_SEED};

const EVENT_FIRE: u64 = 0x4649_5245;
const FIRE_CHANCE: f32 = 0.001;
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
    #[serde(deserialize_with = "deserialize_history")]
    pub events: VecDeque<FireEvent>,
    last_event_seq: u64,
}

fn deserialize_history<'de, D>(deserializer: D) -> Result<VecDeque<FireEvent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct History;

    impl<'de> Visitor<'de> for History {
        type Value = VecDeque<FireEvent>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a sequence of fire events")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut events =
                VecDeque::with_capacity(seq.size_hint().unwrap_or(0).min(EVENT_HISTORY_LIMIT));
            while let Some(event) = seq.next_element()? {
                if events.len() == EVENT_HISTORY_LIMIT {
                    events.pop_front();
                }
                events.push_back(event);
            }
            Ok(events)
        }
    }

    deserializer.deserialize_seq(History)
}

impl Fires {
    /// Iterates over retained events newer than the given sequence, oldest first.
    pub fn events_since(&self, seq: u64) -> impl Iterator<Item = &FireEvent> {
        self.events.iter().filter(move |event| event.seq > seq)
    }

    fn record(&mut self, tick: Tick, building: BuildingID, kind: FireEventKind) {
        self.last_event_seq = self
            .last_event_seq
            .checked_add(1)
            .expect("fire event sequence exhausted");
        let overflow = self.events.len().saturating_sub(EVENT_HISTORY_LIMIT - 1);
        self.events.drain(..overflow);
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
    let tick = elapsed_tick(sim);
    let phase_tick = elapsed_phase_tick(tick.0);
    if !due(phase_tick) {
        return;
    }
    profiling::scope!("souls::events_update");

    let hour = phase_tick / TICKS_PER_HOUR;
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
        super::development::remove_site(sim, id, super::development::RemovalCause::Fire);
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

#[cfg(test)]
mod tests {
    use super::*;
    use common::saveload::{Bincode, Encoder};

    #[test]
    fn pf_d_fire_history_is_bounded_on_load() {
        let mut fires = Fires::default();
        for seq in 1..=300 {
            fires.events.push_back(FireEvent {
                seq,
                tick: Tick(seq),
                building: BuildingID::default(),
                kind: FireEventKind::Ignited,
            });
        }
        fires.last_event_seq = 300;
        let encoded = Bincode::encode(&fires).unwrap();
        let mut restored: Fires = Bincode::decode(&encoded).unwrap();
        assert_eq!(restored.events.len(), EVENT_HISTORY_LIMIT);
        assert_eq!(restored.events.front().unwrap().seq, 45);
        restored.record(
            Tick(301),
            BuildingID::default(),
            FireEventKind::Extinguished,
        );
        assert_eq!(restored.events.len(), EVENT_HISTORY_LIMIT);
        assert_eq!(restored.events.back().unwrap().seq, 301);
    }

    #[test]
    #[should_panic(expected = "fire event sequence exhausted")]
    fn pf_d_fire_sequence_checks_overflow() {
        let mut fires = Fires {
            last_event_seq: u64::MAX,
            ..Fires::default()
        };
        fires.record(Tick(1), BuildingID::default(), FireEventKind::Ignited);
    }
}

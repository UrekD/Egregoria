use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::ops::Bound::{Excluded, Included};

use prototypes::{ServiceKind, Tick, TICKS_PER_HOUR, TICKS_PER_MINUTE};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use slotmapd::Key;

use crate::economy::Budget;
use crate::map::{Building, BuildingID, BuildingKind};
use crate::map_dynamic::{CoverageGrid, EnvGrid};
use crate::{Simulation, SimulationOptions};

const FIRE_IGNITION: u64 = 0x4649_5245_5f49_474e;
const LOSS_MINUTES: u64 = 85;
const MIGRATION_GRACE_MINUTES: u64 = 15;
const ACTIVE_LIMIT: usize = 96;
const LEGACY_LIMIT: usize = 4096;
const PENDING_LIMIT: usize = 512;
const PENDING_TTL: u64 = TICKS_PER_HOUR;
const RISK_BUDGET: usize = 256;
const EVENT_BUDGET: usize = 128;
const EVENT_HISTORY_LIMIT: usize = 256;
#[cfg(test)]
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
    #[serde(deserialize_with = "deserialize_legacy_fires")]
    pub(crate) by_building: BTreeMap<BuildingID, Tick>,
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

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FireIncidentView {
    pub id: u64,
    pub site: BuildingID,
    pub position: geom::Vec3,
    pub offered_tick: Tick,
    pub alarm_tick: Tick,
    pub intensity: u8,
    pub damage: u8,
    last_update: Tick,
    next_update: Tick,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FireCounters {
    pub site_hours: u64,
    pub spontaneous: u64,
    pub admitted: u64,
    pub deferred: u64,
    pub expired: u64,
    pub ineligible: u64,
    pub duplicate: u64,
    pub overflow: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Offer {
    due: Tick,
    site: BuildingID,
}

#[derive(Serialize, Deserialize)]
pub struct ServiceIncidents {
    #[serde(deserialize_with = "deserialize_active")]
    active: BTreeMap<BuildingID, FireIncidentView>,
    #[serde(deserialize_with = "deserialize_pending")]
    pending: BTreeMap<BuildingID, Offer>,
    #[serde(skip)]
    scheduled: BTreeSet<(Tick, BuildingID)>,
    #[serde(skip)]
    offers: BTreeSet<Offer>,
    next_id: u64,
    next_risk_minute: u64,
    cohort_after: Option<BuildingID>,
    pub counters: FireCounters,
}

impl Default for ServiceIncidents {
    fn default() -> Self {
        Self {
            active: BTreeMap::new(),
            pending: BTreeMap::new(),
            scheduled: BTreeSet::new(),
            offers: BTreeSet::new(),
            next_id: 0,
            next_risk_minute: TICKS_PER_HOUR / TICKS_PER_MINUTE,
            cohort_after: None,
            counters: FireCounters::default(),
        }
    }
}

impl ServiceIncidents {
    pub fn active(&self) -> impl Iterator<Item = &FireIncidentView> {
        self.active.values()
    }

    pub fn incident(&self, site: BuildingID) -> Option<&FireIncidentView> {
        self.active.get(&site)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn offer(&mut self, offer: Offer) {
        if self.active.contains_key(&offer.site) {
            self.counters.duplicate += 1;
            return;
        }
        if let Some(old) = self.pending.get(&offer.site).copied() {
            self.counters.duplicate += 1;
            if old <= offer {
                return;
            }
            self.offers.remove(&old);
        } else if self.pending.len() == PENDING_LIMIT {
            self.counters.overflow += 1;
            let last = *self.offers.last().expect("pending queue and index agree");
            if last <= offer {
                return;
            }
            self.remove_offer(last);
        }
        self.offers.insert(offer);
        self.pending.insert(offer.site, offer);
        if self.active.len() >= ACTIVE_LIMIT {
            self.counters.deferred += 1;
        }
    }

    fn remove_offer(&mut self, offer: Offer) {
        self.pending.remove(&offer.site);
        self.offers.remove(&offer);
    }

    fn start(&mut self, offer: Offer, tick: Tick, position: geom::Vec3) {
        self.next_id += 1;
        let next_update = Tick(tick.0.saturating_add(TICKS_PER_MINUTE));
        self.active.insert(
            offer.site,
            FireIncidentView {
                id: self.next_id,
                site: offer.site,
                position,
                offered_tick: offer.due,
                alarm_tick: tick,
                intensity: 1,
                damage: 0,
                last_update: tick,
                next_update,
            },
        );
        self.scheduled.insert((next_update, offer.site));
        self.counters.admitted += 1;
    }

    fn remove_active(&mut self, site: BuildingID) {
        if let Some(incident) = self.active.remove(&site) {
            self.scheduled.remove(&(incident.next_update, site));
        }
    }
}

pub fn fire_incident(sim: &Simulation, site: BuildingID) -> Option<FireIncidentView> {
    sim.read::<ServiceIncidents>().active.get(&site).copied()
}

pub(crate) fn remove_incident(sim: &Simulation, site: BuildingID) {
    let mut incidents = sim.write::<ServiceIncidents>();
    incidents.remove_active(site);
    if let Some(offer) = incidents.pending.get(&site).copied() {
        incidents.remove_offer(offer);
        incidents.counters.ineligible += 1;
    }
}

pub(crate) fn elapsed_tick(sim: &Simulation) -> Tick {
    Tick(sim.read::<Budget>().elapsed_ticks(Tick(sim.get_tick())))
}

pub fn events_update(sim: &mut Simulation) {
    let tick = elapsed_tick(sim);
    let mut losses = Vec::new();
    {
        let mut incidents = sim.write::<ServiceIncidents>();
        if incidents.next_risk_minute > tick.0 / TICKS_PER_MINUTE
            && incidents
                .scheduled
                .first()
                .is_none_or(|(due, _)| *due > tick)
            && incidents.offers.first().is_none_or(|offer| {
                incidents.active.len() >= ACTIVE_LIMIT
                    && tick.0.saturating_sub(offer.due.0) < PENDING_TTL
            })
        {
            return;
        }
        profiling::scope!("souls::events_update");
        let map = sim.map();
        let mut history = sim.write::<Fires>();

        if incidents.next_risk_minute <= tick.0 / TICKS_PER_MINUTE {
            sample_risk(sim, &map, &mut incidents);
        }
        for _ in 0..EVENT_BUDGET {
            let Some((due, id)) = incidents.scheduled.first().copied() else {
                break;
            };
            if due > tick {
                break;
            }
            incidents.scheduled.pop_first();
            let Some(building) = map.get(id) else {
                incidents.active.remove(&id);
                continue;
            };
            let incident = incidents.active.get_mut(&id).expect("scheduled incident");
            let minutes = tick.0.saturating_sub(incident.last_update.0) / TICKS_PER_MINUTE;
            let working = sim.read::<super::service_dispatch::FireFleet>()
                .suppression_started(id, incident.id);
            let working_minutes = working.map_or(0, |start| tick.0.saturating_sub(start.0) / TICKS_PER_MINUTE);
            if working_minutes >= super::service_dispatch::SUPPRESSION_MINUTES {
                incidents.active.remove(&id);
                history.record(tick, id, FireEventKind::Extinguished);
                continue;
            }
            let exposed_minutes = working.map_or(minutes, |start| {
                start.0.saturating_sub(incident.last_update.0) / TICKS_PER_MINUTE
            }).min(minutes);
            incident.damage = (u64::from(incident.damage) + exposed_minutes).min(LOSS_MINUTES) as u8;
            if incident.damage == LOSS_MINUTES as u8 {
                incidents.active.remove(&id);
                history.record(tick, id, FireEventKind::Collapsed);
                losses.push(id);
                continue;
            }
            incident.position = building.obb.center().z(building.height);
            incident.intensity = (1 + incident.damage / 10).min(9)
                .saturating_sub(working_minutes as u8).max(1);
            incident.last_update.0 += minutes * TICKS_PER_MINUTE;
            incident.next_update = Tick(incident.last_update.0.saturating_add(TICKS_PER_MINUTE));
            let next_update = incident.next_update;
            incidents.scheduled.insert((next_update, id));
        }
        for _ in 0..EVENT_BUDGET {
            let Some(offer) = incidents.offers.first().copied() else {
                break;
            };
            if tick.0.saturating_sub(offer.due.0) >= PENDING_TTL {
                incidents.remove_offer(offer);
                incidents.counters.expired += 1;
                continue;
            }
            if incidents.active.len() >= ACTIVE_LIMIT {
                break;
            }
            incidents.remove_offer(offer);
            let Some(site) = map.get(offer.site).filter(|building| eligible(building)) else {
                incidents.counters.ineligible += 1;
                continue;
            };
            incidents.start(offer, tick, site.obb.center().z(site.height));
            history.record(tick, offer.site, FireEventKind::Ignited);
        }
    }
    for id in losses {
        super::demolition::ruin_site(sim, id, super::demolition::SiteRemnantCause::Fire)
            .expect("an incident loss retains its occupied site");
    }
}

fn sample_risk(sim: &Simulation, map: &crate::map::Map, incidents: &mut ServiceIncidents) {
    let minute = incidents.next_risk_minute;
    let cohort = (minute % 60) as u8;
    let from = incidents.cohort_after.map_or(
        Included((cohort, BuildingID::from(slotmapd::KeyData::from_ffi(0)))),
        |id| Excluded((cohort, id)),
    );
    // Use an exclusive next-cohort sentinel to avoid depending on the maximum slotmap key.
    let mut candidates = map.fire_cohorts.range((
        from,
        Excluded((cohort + 1, BuildingID::from(slotmapd::KeyData::from_ffi(0)))),
    ));
    let coverage = sim.read::<CoverageGrid>();
    let environment = sim.read::<EnvGrid>();
    let seed = sim.read::<SimulationOptions>().seed;
    let denominator = prototypes::try_prototype(prototypes::ServiceID::new("fire-station"))
        .and_then(|service| service.fire)
        .map_or(10_000, |fire| fire.ignition_hourly_denominator);
    for &(_, site) in candidates.by_ref().take(RISK_BUDGET) {
        incidents.cohort_after = Some(site);
        let Some(building) = map.get(site).filter(|b| eligible(b)) else {
            continue;
        };
        incidents.counters.site_hours += 1;
        let pollution = environment
            .pollution_at(building.obb.center())
            .clamp(0.0, 100.0) as u64;
        let prevention = coverage.coverage(site, ServiceKind::Fire).clamp(0.0, 100.0) as u64;
        // Quantize both inputs by rounding down to whole points. Pollution gives up to 3x
        // risk and full prevention removes 35% of risk. Round the final threshold down.
        let threshold =
            ((1u128 << 64) * u128::from(50 + pollution) * u128::from(10_000 - 35 * prevention))
                / (u128::from(denominator) * 500_000);
        let roll = [seed, minute / 60, site.data().as_ffi(), FIRE_IGNITION]
            .into_iter()
            .fold(0, |state, field| splitmix64(state ^ field));
        if u128::from(roll) < threshold {
            incidents.counters.spontaneous += 1;
            if benchmark_suppresses_fire_offers() {
                continue;
            }
            incidents.offer(Offer {
                due: Tick(minute * TICKS_PER_MINUTE),
                site,
            });
        }
    }
    if candidates.next().is_none() {
        incidents.next_risk_minute += 1;
        incidents.cohort_after = None;
    }
}

// Only the source-pinned background benchmark sets this process option.
fn benchmark_suppresses_fire_offers() -> bool {
    static BACKGROUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *BACKGROUND.get_or_init(|| {
        std::env::var_os("EGR_P9_BENCH_PROFILE").is_some_and(|value| value == "background")
    })
}

fn eligible(building: &Building) -> bool {
    can_burn(building.kind) && !building.is_abandoned_growable()
}

pub(crate) fn can_burn(kind: BuildingKind) -> bool {
    kind == BuildingKind::House
        || kind.as_goods_company().is_some_and(|id| {
            prototypes::try_prototype(prototypes::GrowableCompanyID::from(id))
                .is_some_and(|firm| firm.growable)
        })
}

pub(crate) fn cohort_key(site: BuildingID) -> (u8, BuildingID) {
    ((splitmix64(site.data().as_ffi()) % 60) as u8, site)
}

fn splitmix64(value: u64) -> u64 {
    let value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// Trusted scenario input. Player and network commands cannot offer fires.
pub fn offer_fixture_fire(sim: &Simulation, site: BuildingID) -> bool {
    if sim.map().get(site).is_none_or(|b| !eligible(b)) {
        return false;
    }
    sim.write::<ServiceIncidents>().offer(Offer {
        due: elapsed_tick(sim),
        site,
    });
    true
}

#[cfg(test)]
pub(crate) fn force_ignite(sim: &mut Simulation, building: BuildingID) {
    offer_fixture_fire(sim, building);
    events_update(sim);
}

pub(crate) fn restore_incidents(sim: &Simulation, legacy: bool) -> Result<(), &'static str> {
    let now = elapsed_tick(sim);
    let map = sim.map();
    let mut history = sim.write::<Fires>();
    let mut incidents = sim.write::<ServiceIncidents>();
    if legacy {
        let mut fires: Vec<_> = std::mem::take(&mut history.by_building)
            .into_iter()
            .collect();
        fires.sort_unstable_by_key(|&(site, ignition)| (ignition, site));
        for (index, (site, ignition)) in fires.into_iter().enumerate() {
            let Some(building) = map.get(site).filter(|b| can_burn(b.kind)) else {
                incidents.counters.ineligible += 1;
                continue;
            };
            if ignition > now {
                return Err("legacy fire starts after the snapshot clock");
            }
            incidents.start(
                Offer {
                    due: ignition,
                    site,
                },
                ignition,
                building.obb.center().z(building.height),
            );
            let active = incidents.active.get_mut(&site).unwrap();
            active.damage = ((now.0 - ignition.0) / TICKS_PER_MINUTE)
                .min(LOSS_MINUTES - MIGRATION_GRACE_MINUTES) as u8;
            active.intensity = (1 + active.damage / 10).min(9);
            active.last_update = now;
            active.next_update = Tick(now.0.saturating_add(1 + index as u64 / EVENT_BUDGET as u64));
        }
        incidents.next_risk_minute = (now.0 / TICKS_PER_MINUTE + 1).max(60);
    } else if !history.by_building.is_empty() {
        return Err("current snapshot has conflicting legacy fire authority");
    }
    if !(60..=(now.0 / TICKS_PER_MINUTE + 1).max(60)).contains(&incidents.next_risk_minute)
        || incidents
            .cohort_after
            .is_some_and(|site| u64::from(cohort_key(site).0) != incidents.next_risk_minute % 60)
        || history.last_event_seq == u64::MAX
        || history
            .events
            .iter()
            .any(|event| event.seq == 0 || event.seq > history.last_event_seq || event.tick > now)
        || history
            .events
            .iter()
            .zip(history.events.iter().skip(1))
            .any(|(a, b)| a.seq >= b.seq || a.tick > b.tick)
        || incidents.next_id == u64::MAX
    {
        return Err("invalid fire history or sequence");
    }
    let mut ids = BTreeSet::new();
    let mut scheduled = BTreeSet::new();
    for (&site, incident) in &incidents.active {
        if map
            .get(site)
            .is_none_or(|b| !can_burn(b.kind) && b.kind != BuildingKind::SiteRemnant)
            || incident.site != site
            || incident.id == 0
            || incident.id > incidents.next_id
            || !ids.insert(incident.id)
            || !incident.position.is_finite()
            || incident.damage >= LOSS_MINUTES as u8
            || !(1..=9).contains(&incident.intensity)
            || incident.offered_tick > incident.alarm_tick
            || incident.alarm_tick > incident.last_update
            || incident.last_update > now
            || incident.next_update <= incident.last_update
        {
            return Err("invalid active fire authority");
        }
        scheduled.insert((incident.next_update, site));
    }
    for (&site, offer) in &incidents.pending {
        if offer.site != site || offer.due > now || incidents.active.contains_key(&site) {
            return Err("invalid pending fire authority");
        }
    }
    incidents.scheduled = scheduled;
    incidents.offers = incidents.pending.values().copied().collect();
    Ok(())
}

fn deserialize_legacy_fires<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<BuildingID, Tick>, D::Error> {
    deserialize_map::<_, _, LEGACY_LIMIT>(d)
}

fn deserialize_active<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<BuildingID, FireIncidentView>, D::Error> {
    deserialize_map::<_, _, LEGACY_LIMIT>(d)
}

fn deserialize_pending<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<BuildingID, Offer>, D::Error> {
    deserialize_map::<_, _, PENDING_LIMIT>(d)
}

pub(super) fn deserialize_map<'de, D, V, const LIMIT: usize>(
    d: D,
) -> Result<BTreeMap<BuildingID, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct BoundedMap<V, const N: usize>(std::marker::PhantomData<V>);
    impl<'de, V: Deserialize<'de>, const N: usize> Visitor<'de> for BoundedMap<V, N> {
        type Value = BTreeMap<BuildingID, V>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {N} unique site entries")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            if input.size_hint().is_some_and(|len| len > N) {
                return Err(A::Error::custom("site entry limit exceeded"));
            }
            let mut entries = BTreeMap::new();
            while let Some(key) = input.next_key()? {
                if entries.len() == N || entries.contains_key(&key) {
                    return Err(A::Error::custom("too many or duplicate site entries"));
                }
                entries.insert(key, input.next_value()?);
            }
            Ok(entries)
        }
    }
    d.deserialize_map(BoundedMap::<V, LIMIT>(std::marker::PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::saveload::{Bincode, Encoder};

    #[test]
    fn p9_pending_offers_keep_the_earliest_keys_without_renewing_duplicates() {
        let mut incidents = ServiceIncidents::default();
        let site = |index| BuildingID::from(slotmapd::KeyData::from_ffi((1u64 << 32) | index));
        for i in (1..=PENDING_LIMIT as u64 + 1).rev() {
            incidents.offer(Offer {
                due: Tick(i),
                site: site(i),
            });
        }
        let first = *incidents.offers.first().unwrap();
        incidents.offer(Offer {
            due: Tick(700),
            site: first.site,
        });
        assert_eq!(incidents.pending_count(), PENDING_LIMIT);
        assert_eq!(
            incidents.offers.last().unwrap().due,
            Tick(PENDING_LIMIT as u64)
        );
        assert_eq!(incidents.pending[&first.site].due, first.due);
        assert_eq!(incidents.counters.overflow, 1);
        assert_eq!(incidents.counters.duplicate, 1);
    }

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

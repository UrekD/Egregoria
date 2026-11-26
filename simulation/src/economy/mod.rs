//! Economy module
//!
//! This module contains all the code related to the economy of the game.
//!
//! The economy is divided in 2 parts:
//!
//! - The market, which is the place where goods are exchanged.
//! - The government, which is the entity representing the player
//!
use crate::utils::resources::Resources;
use crate::SoulID;
use crate::World;
use egui_inspect::Inspect;
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

pub mod balance;
mod budget;
pub mod cargo;
mod citystats;
mod demand;
mod ecostats;
mod government;
mod market;
pub mod population;
pub mod progression;
pub mod transit_stats;
mod trip_stats;

use crate::map::{BuildingKind, LaneID, LaneKind, Map, TraverseDirection};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::BuildingInfos;
use crate::souls::household::settle_local_pickup;
use crate::transportation::Location;
use crate::world::HumanID;
pub use budget::*;
pub use citystats::*;
pub use demand::*;
pub use ecostats::*;
pub use government::*;
pub use market::*;
pub use transit_stats::TransitStats;
pub use trip_stats::*;
use prototypes::{GameTime, ItemID, Money};

const WORKER_CONSUMPTION_PER_MINUTE: Money = Money::new_cents(10);

#[derive(Inspect, Default, Serialize, Deserialize)]
pub struct Sold(pub Vec<Trade>);

#[derive(Inspect, Default, Serialize, Deserialize)]
pub struct Bought(pub BTreeMap<ItemID, Vec<Trade>>);

#[derive(Inspect, Debug, Default, Serialize, Deserialize)]
pub struct Workers(pub Vec<HumanID>);

#[derive(Default)]
pub(crate) struct WalkingComponents {
    generation: u64,
    labels: BTreeMap<LaneID, LaneID>,
}

static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

/// A value with a process-unique stamp, renewed on every mutable access.
/// A cache keyed by the stamp stays valid while the value is unchanged.
/// Serializes as the bare value.
pub(crate) struct Stamped<T> {
    value: T,
    stamp: u64,
}

impl<T> Stamped<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            stamp: NEXT_STAMP.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub(crate) fn stamp(&self) -> u64 {
        self.stamp
    }
}

impl<T: Default> Default for Stamped<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> Deref for Stamped<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Stamped<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.stamp = NEXT_STAMP.fetch_add(1, Ordering::Relaxed);
        &mut self.value
    }
}

impl<'a, T> IntoIterator for &'a Stamped<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.value.into_iter()
    }
}

impl<T: Serialize> Serialize for Stamped<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Stamped<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::new)
    }
}

/// Sorted set of every worker on a company roster, rebuilt only when a roster changes.
/// Also keeps the job applicants found for one state of the job buy orders.
#[derive(Default)]
pub(crate) struct JobRosters {
    flat: Vec<HumanID>,
    sorted: Vec<HumanID>,
    /// Stamp of the job buy orders that `applicants` and `stale` were collected from.
    orders: Option<u64>,
    applicants: Vec<HumanID>,
    stale: Vec<SoulID>,
}

impl JobRosters {
    /// Returns the sorted, deduplicated workers. The second value is true when rebuilt.
    pub(crate) fn refresh(&mut self, world: &World) -> (&[HumanID], bool) {
        let workers = || {
            world
                .companies
                .values()
                .flat_map(|company| company.workers.0.iter().copied())
        };
        let rebuilt = !workers().eq(self.flat.iter().copied());
        if rebuilt {
            self.flat.clear();
            self.flat.extend(workers());
            self.sorted.clone_from(&self.flat);
            self.sorted.sort_unstable();
            self.sorted.dedup();
        }
        (&self.sorted, rebuilt)
    }
}

impl EcoStats {
    /// Returns the retail units collected by residents in the longest history window.
    pub fn retail_fulfilled(&self) -> i64 {
        let retail = ItemID::new("retail-goods");
        self.internal_trade
            .iter_histories(LEVEL_FREQS.len() - 1)
            .find(|(id, _)| *id == retail)
            .map_or(0, |(_, history)| history.past_ring_items.iter().sum())
    }
}

pub fn market_update(world: &mut World, resources: &mut Resources) {
    profiling::scope!("economy::market_update");
    let mut m = resources.write::<Market>();
    let job_opening = ItemID::new("job-opening");
    let tick = resources.read::<GameTime>().tick;

    let mut trades = m.prepare_job_matches(world, resources, job_opening, tick);
    let freights = &world.freight_stations;
    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let mut components = resources.write::<WalkingComponents>();
    if components.generation != map.walking_topology_generation {
        components.labels.clear();
        components.generation = map.walking_topology_generation;
    }
    let mut buyer_lanes = BTreeMap::new();
    let mut seller_lanes = BTreeMap::new();
    trades.extend_from_slice(m.make_trades_filtered(
        |pos| {
            freights
                .iter()
                .filter_map(|(id, freight)| {
                    let building = map.get(freight.f.building)?;
                    (matches!(building.kind, BuildingKind::RailFreightStation(_))
                        && infos.owner(building.id) == Some(SoulID::FreightStation(id)))
                    .then_some((OrderedFloat(building.door_pos.xy().distance2(pos)), id))
                })
                .min()
                .map(|(_, id)| SoulID::FreightStation(id))
        },
        |buyer, seller| {
            let SoulID::Human(buyer) = buyer else {
                return true;
            };
            let buyer_route = *buyer_lanes.entry(buyer).or_insert_with(|| {
                let human = world.humans.get(buyer)?;
                let pos = match human.location {
                    Location::Outside => human.trans.pos,
                    Location::Building(id) => map.get(id)?.door_pos,
                    Location::Vehicle(id) | Location::Transit(id) => {
                        world.vehicles.get(id)?.trans.pos
                    }
                };
                let lane = map.nearest_lane(pos, LaneKind::Walking, None)?;
                Some((lane, human.goods.excluded_seller()))
            });
            let Some((buyer_lane, excluded_seller)) = buyer_route else {
                return false;
            };
            if excluded_seller == Some(seller) {
                return false;
            }
            let seller_lane = *seller_lanes.entry(seller).or_insert_with(|| {
                let store = map.get(infos.building_owned_by(seller)?)?;
                let road = map.get(store.connected_road?)?;
                road.lanes_iter()
                    .filter(|&(_, kind)| kind == LaneKind::Walking)
                    .map(|(id, _)| id)
                    .min_by_key(|&id| {
                        let distance = map.lanes()[id].points.project_dist2(store.door_pos);
                        (OrderedFloat(distance), id)
                    })
            });
            seller_lane.is_some_and(|seller_lane| {
                walking_component(&map, &mut components.labels, buyer_lane)
                    == walking_component(&map, &mut components.labels, seller_lane)
            })
        },
        &BTreeMap::new(),
    ));
    drop((m, map, infos, components));

    let retail = ItemID::new("retail-goods");
    // A hook that returns true owns the whole settlement: budget, stock and the
    // trade's EcoStats entry via `EcoStats::record_trade` when it completes.
    // Recording here as well would count the trade twice.
    trades.retain(|&trade| {
        if trade.kind == job_opening {
            return true;
        }
        if matches!(trade.buyer.0, SoulID::Human(_)) {
            return !settle_local_pickup(world, resources, trade);
        }
        !cargo::settle_physical_match(world, resources, trade)
    });

    for &trade in &trades {
        log::debug!("A trade was made! {:?}", trade);

        if trade.kind == job_opening {
            if let SoulID::GoodsCompany(id) = trade.seller.0 {
                let comp = world.companies.get_mut(id).unwrap();
                comp.workers.0.push(trade.buyer.0.try_into().unwrap())
            }
        }
        let category = if trade.money_delta < Money::ZERO {
            BudgetCategory::TradeImport
        } else {
            BudgetCategory::TradeExport
        };
        Budget::apply(resources, category, trade.money_delta);

        if let SoulID::GoodsCompany(id) = trade.seller.0 {
            if trade.kind != job_opening {
                world.companies.get_mut(id).unwrap().sold.0.push(trade);
            }
        }

        match trade.buyer.0 {
            SoulID::Human(id) => {
                world.edit_human(id, |h| {
                    h.bought.0.entry(trade.kind).or_default().push(trade);
                });
            }
            SoulID::GoodsCompany(id) => {
                if let Some(c) = world.companies.get_mut(id) {
                    c.bought.0.entry(trade.kind).or_default().push(trade)
                }
            }
            SoulID::FreightStation(_) => {}
        }
    }
    trades.retain(|trade| trade.kind != retail || !matches!(trade.buyer.0, SoulID::Human(_)));
    resources.write::<EcoStats>().advance(tick.0, &trades);

    let open_day = elapsed_ticks(resources).0 / TICKS_PER_BUDGET_DAY;
    resources
        .write::<DailyEconomyMetrics>()
        .retain_recent(open_day);
}

/// Labels walking lanes connected by sidewalk corners and crossings for the current topology.
fn walking_component(
    map: &Map,
    components: &mut BTreeMap<LaneID, LaneID>,
    start: LaneID,
) -> LaneID {
    if let Some(&component) = components.get(&start) {
        return component;
    }
    let mut pending = vec![start];
    components.insert(start, start);
    while let Some(id) = pending.pop() {
        let lane = &map.lanes()[id];
        for intersection in [lane.src, lane.dst] {
            for (turn, direction) in map.intersections()[intersection].turns_from(id) {
                let adjacent = match direction {
                    TraverseDirection::Forward => turn.dst,
                    TraverseDirection::Backward => turn.src,
                };
                if map.lanes()[adjacent].kind == LaneKind::Walking {
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        components.entry(adjacent)
                    {
                        entry.insert(start);
                        pending.push(adjacent);
                    }
                }
            }
        }
    }
    start
}

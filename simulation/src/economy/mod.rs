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

pub mod balance;
mod budget;
mod citystats;
mod demand;
mod ecostats;
mod government;
mod market;

use crate::map::{BuildingKind, LaneID, LaneKind, Map, TraverseDirection};
use crate::map_dynamic::BuildingInfos;
use crate::transportation::Location;
use crate::world::HumanID;
pub use budget::*;
pub use citystats::*;
pub use demand::*;
pub use ecostats::*;
pub use government::*;
pub use market::*;
use prototypes::{GameTime, ItemID, Money};

const WORKER_CONSUMPTION_PER_MINUTE: Money = Money::new_cents(10);

#[derive(Inspect, Default, Serialize, Deserialize)]
pub struct Sold(pub Vec<Trade>);

#[derive(Inspect, Default, Serialize, Deserialize)]
pub struct Bought(pub BTreeMap<ItemID, Vec<Trade>>);

#[derive(Inspect, Debug, Default, Serialize, Deserialize)]
pub struct Workers(pub Vec<HumanID>);

pub fn market_update(world: &mut World, resources: &mut Resources) {
    profiling::scope!("economy::market_update");
    let mut m = resources.write::<Market>();
    let job_opening = ItemID::new("job-opening");
    let tick = resources.read::<GameTime>().tick;

    let freights = &world.freight_stations;

    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let mut buyer_lanes = BTreeMap::new();
    let mut seller_lanes = BTreeMap::new();
    let mut components = BTreeMap::new();
    let trades = m.make_trades_filtered(
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
            let buyer_lane = *buyer_lanes.entry(buyer).or_insert_with(|| {
                let human = world.humans.get(buyer)?;
                let pos = match human.location {
                    Location::Outside => human.trans.pos,
                    Location::Building(id) => map.get(id)?.door_pos,
                    Location::Vehicle(id) => world.vehicles.get(id)?.trans.pos,
                };
                map.nearest_lane(pos, LaneKind::Walking, None)
            });
            let seller_lane = *seller_lanes.entry(seller).or_insert_with(|| {
                let store = map.get(infos.building_owned_by(seller)?)?;
                store.connected_road?;
                map.nearest_lane(store.door_pos, LaneKind::Walking, None)
            });
            buyer_lane.zip(seller_lane).is_some_and(|(buyer, seller)| {
                walking_component(&map, &mut components, buyer)
                    == walking_component(&map, &mut components, seller)
            })
        },
    );

    resources.write::<EcoStats>().advance(tick.0, trades);

    for &trade in trades.iter() {
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
                if let Some(h) = world.humans.get_mut(id) {
                    h.bought.0.entry(trade.kind).or_default().push(trade);
                }
            }
            SoulID::GoodsCompany(id) => {
                if let Some(c) = world.companies.get_mut(id) {
                    c.bought.0.entry(trade.kind).or_default().push(trade)
                }
            }
            SoulID::FreightStation(_) => {}
        }
    }
}

/// Labels walking lanes connected by sidewalk corners and crossings, once per match pass.
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

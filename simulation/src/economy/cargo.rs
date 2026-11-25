//! Road freight owns stock from reservation through delivery. Market capital is on-hand stock.
//!
//! | Lane | Stock handoff | City budget and rail counter |
//! | --- | --- | --- |
//! | Virtual local (including growables with road freight disabled) | Market match | Neither |
//! | Virtual station export | Market match | Export credit + waiting units |
//! | Station import | Ordered by rail, unloaded into station stock by a train call, carried by a station truck, buyer credited at unload | Import debit (value + freight) at unload, wanted units until the train call |
//! | Producer export without a dispatchable truck or road route | Direct export at match, after local recipes keep one truckload | Export credit + waiting units |
//! | Physical company → company | Source debited at reservation, destination credited at verified unload | Neither |
//! | Physical company → station | Source debited at reservation, station receives at verified unload | Export credit + waiting units at unload |
//!
//! Blocked routes release their stock first. A live station can then handle a
//! separate virtual trade. Cancellation never posts an import or export.
//! A recipe input that no truck brings is imported from the nearest station with
//! train service that reaches the buyer by road. Without one there is no import.
//! An outbound leg that keeps failing in the router suspends its route until the
//! cooldown ends or the road network changes. While the consumer is still in
//! reach, its source keeps the reorder on hand, so only stock an open route
//! would leave over takes another lane. Open routes are served before stock is
//! held for suspended ones. A loaded truck that stops moving releases its load,
//! and a load past its due time no longer keeps the recipient from importing.

use std::collections::{BTreeMap, BTreeSet};

use ordered_float::OrderedFloat;
use prototypes::{
    game_secs_to_ticks, DayTime, GameTime, GoodsCompanyPrototype, ItemID, Money, Tick,
    WarehouseSpec, SECONDS_PER_HOUR, TICKS_PER_HOUR, TICKS_PER_MINUTE, TICKS_PER_REALTIME_SECOND,
};
use serde::{Deserialize, Serialize};

use super::{
    Budget, BudgetCategory, EcoStats, FreightDay, Market, Trade, TradeTarget, TICKS_PER_BUDGET_DAY,
};
use crate::map::{BuildingID, LaneKind, Map, PathKind, RoadID};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{
    active_business, ActiveBusiness, BuildingInfos, Destination, Itinerary, ParkingManagement,
};
use crate::souls::desire::WorkKind;
use crate::souls::freight_station::train_stop;
use crate::souls::household::household_items;
use crate::transportation::{
    spawn_truck_at_depot, unpark, Location, TransportGrid, VehicleKind, VehicleState,
};
use crate::utils::resources::Resources;
use crate::world::FreightStationEnt;
use crate::{
    CompanyID, HumanEnt, HumanID, ParCommandBuffer, Simulation, SoulID, VehicleEnt, VehicleID,
    World,
};

pub use prototypes::WarehousePolicy;

// Matching runs on game-minute boundaries, where the retry windows land (two real seconds).
const FREIGHT_MATCH_INTERVAL: u64 = TICKS_PER_MINUTE;
// Loading and parking slack is truck movement, so real time.
const DELIVERY_LEAD: u64 = 6 * 60 * TICKS_PER_REALTIME_SECOND;
// The standard ten-hour driver shift leaves a fourteen-hour replenishment gap.
const BETWEEN_DRIVER_SHIFTS: u64 = 14 * TICKS_PER_HOUR;
const DELIVERY_EXPIRY: u64 = 6 * TICKS_PER_HOUR;
const FREIGHT_PACE_METERS_PER_SECOND: f32 = 8.0;
const HANDLING_TICKS: u64 = 30 * TICKS_PER_REALTIME_SECOND;
const DEPARTURES_PER_DRIVER: u32 = 2;
// Router errors such as a full car park clear on their own. Give up on a leg
// only after it has failed continuously for this long.
const ROUTING_RETRY_WINDOW: u64 = 10 * TICKS_PER_MINUTE;
// A route whose outbound leg gave up is skipped by matching for this long.
const FAILED_ROUTE_COOLDOWN: u64 = 2 * TICKS_PER_HOUR;
const FREIGHT_DAYS_KEPT: u64 = 8;
// A loaded truck that has not moved this far for this long is stuck in a jam,
// and its load is released. Movement is real time: two real minutes.
const LOAD_STALL_TICKS: u64 = 60 * TICKS_PER_MINUTE;
const STALL_DISTANCE: f32 = 5.0;
/// Import units one freight train call unloads at a station.
pub const TRAIN_CAPACITY: u32 = 100;
const STATION_TRUCKS: usize = 6;
// Round trips take three times the game time on the faster clock, so a truck
// carries three times as much, like the coal mine's.
const STATION_TRUCK_CAPACITY: u32 = 48;
/// The import lead time a reorder plans for: a train call, then a truck trip
/// in town traffic. Vehicles drive in real time while game time runs faster,
/// so this is real time: 24 real minutes, twelve game hours.
const IMPORT_LEAD: u64 = 24 * 60 * TICKS_PER_REALTIME_SECOND;
/// An import order open this long means imports do not keep up with demand.
/// The wait is train and truck travel, so real time: 12 real minutes, six game hours.
pub const IMPORT_WAIT_WARNING: u64 = 12 * 60 * TICKS_PER_REALTIME_SECOND;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ShipmentId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LoadId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemQuantity {
    pub item: ItemID,
    pub units: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Endpoint {
    soul: SoulID,
    building: BuildingID,
}

impl Endpoint {
    fn live(self, world: &World, map: &Map, infos: &BuildingInfos) -> bool {
        let Some(building) = map.get(self.building) else {
            return false;
        };
        match self.soul {
            SoulID::GoodsCompany(id) => active_business(building, infos, world)
                .is_some_and(|business| business.company == id),
            SoulID::FreightStation(id) => {
                infos.owner(self.building) == Some(self.soul)
                    && world
                        .freight_stations
                        .get(id)
                        .is_some_and(|entity| entity.f.building == self.building)
            }
            SoulID::Human(_) => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Shipment {
    source: Endpoint,
    destination: Endpoint,
    quantity: ItemQuantity,
    reserved_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TruckLoad {
    pub vehicle: VehicleID,
    pub items: Vec<ItemQuantity>,
    source: Endpoint,
    destination: Endpoint,
    driver: HumanID,
    departed_at: u64,
    due_at: u64,
    used_truck: bool,
    routing_error_since: Option<u64>,
}

impl TruckLoad {
    pub fn source(&self) -> BuildingID {
        self.source.building
    }

    pub fn destination(&self) -> BuildingID {
        self.destination.building
    }

    fn units(&self) -> u32 {
        self.items.iter().map(|line| line.units).sum()
    }
}

/// An empty truck driving back to its depot after a verified unload.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct EmptyReturn {
    driver: HumanID,
    depot: Endpoint,
    started_at: u64,
    routing_error_since: Option<u64>,
}

/// A route whose outbound leg failed in the router until the retry window ran out.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct SuspendedRoute {
    until: u64,
    // Unused: a road edit lifts every suspension instead, since the topology
    // generation restarts at load. Kept so the save layout does not change.
    topology: u64,
    roads: [Option<RoadID>; 2],
}

/// Where a loaded truck last moved on its way, and when.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct LoadProgress {
    at: geom::Vec3,
    since: u64,
}

/// Import stock and open orders at one freight station.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ImportDesk {
    // Saves use Bincode varints. Values that fit in u32 retain their encoding.
    /// Units unloaded from trains that wait for a station truck.
    stock: BTreeMap<ItemID, u64>,
    /// Units ordered by rail that no train has unloaded yet.
    on_rail: BTreeMap<ItemID, u64>,
    /// Open orders, oldest first.
    orders: Vec<ImportOrder>,
}

impl ImportDesk {
    fn ordered(&self, item: ItemID) -> u64 {
        self.orders
            .iter()
            .filter(|order| order.quantity.item == item)
            .map(|order| u64::from(order.quantity.units))
            .sum()
    }

    /// Opens an order and puts any part the station will not hold on order by rail.
    fn place(&mut self, order: ImportOrder) {
        self.orders.push(order);
        let item = order.quantity.item;
        let covered = self.stock.get(&item).copied().unwrap_or(0)
            + self.on_rail.get(&item).copied().unwrap_or(0);
        let shortfall = self.ordered(item).saturating_sub(covered);
        if shortfall > 0 {
            *self.on_rail.entry(item).or_default() += shortfall;
        }
    }

    fn on_rail_units(&self) -> u64 {
        self.on_rail.values().sum()
    }

    fn wanted_cargo(&self) -> u32 {
        // FreightStation uses this as a dispatch hint. The desk owns the full backlog.
        bounded_transfer(self.on_rail_units(), u32::MAX)
    }

    fn restock(&mut self, items: &[ItemQuantity]) {
        for line in items {
            *self.stock.entry(line.item).or_default() += u64::from(line.units);
        }
    }
}

fn bounded_transfer(available: u64, capacity: u32) -> u32 {
    u32::try_from(available.min(u64::from(capacity))).unwrap()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct ImportOrder {
    buyer: Endpoint,
    quantity: ItemQuantity,
    ordered_at: u64,
}

/// A truck of a station's own fleet. Stations drive their trucks themselves,
/// as they do their trains.
#[derive(Debug, Serialize, Deserialize)]
struct StationTruck {
    station: Endpoint,
    trip: Option<ImportTrip>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ImportTrip {
    buyer: Endpoint,
    /// Empty on the way back to the station.
    items: Vec<ItemQuantity>,
    ordered_at: u64,
    departed_at: u64,
    returning: bool,
    routing_error_since: Option<u64>,
    /// Where the truck last moved on this leg, for the stall check loads use.
    progress: Option<LoadProgress>,
}

/// Why a company's recipe input is not here yet, as far as imports go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportStatus {
    /// No freight station with train service reaches the company by road.
    NoRoute,
    /// An import has been on order since this elapsed tick.
    Waiting { since: u64 },
}

/// Companies whose imports are failing, for the advisor.
#[derive(Debug, Default)]
pub struct ImportProblems {
    /// Short of an input with no import route.
    pub no_route: Vec<BuildingID>,
    /// Waiting longer than [`IMPORT_WAIT_WARNING`] for an import.
    pub slow: Vec<BuildingID>,
}

/// What a warehouse is doing, or why it is idle, as of the last freight match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarehouseActivity {
    /// One of its trucks or a delivery to it is on the road.
    Moving,
    /// Cargo to or from it waits for a driver at the loading dock.
    Queued,
    /// No worker is assigned to drive its trucks.
    NoWorkers,
    /// It is restocking, but no producer of a stored item reaches it by road.
    NoSupplierInReach,
    /// It is restocking, and the producers in reach have nothing to spare yet.
    WaitingForSupply,
    /// Stock is above the restock level and no reachable recipe needs it.
    NothingBelowWatermark,
}

#[derive(Serialize, Deserialize)]
pub struct CargoBook {
    next_id: u64,
    shipments: BTreeMap<ShipmentId, Shipment>,
    loads: BTreeMap<LoadId, TruckLoad>,
    policies: BTreeMap<CompanyID, WarehousePolicy>,
    restocking: BTreeSet<CompanyID>,
    pub(crate) growable_freight: bool,
    returning: BTreeMap<VehicleID, EmptyReturn>,
    suspended: BTreeMap<(Endpoint, Endpoint), SuspendedRoute>,
    days: BTreeMap<u64, FreightDay>,
    imports: BTreeMap<Endpoint, ImportDesk>,
    station_trucks: BTreeMap<VehicleID, StationTruck>,
    /// The last import delivery of each recipe input, kept for a day.
    import_customers: BTreeMap<(Endpoint, ItemID), u64>,
    #[serde(skip)]
    routes: RouteCache,
    // Derived at every match, so a loaded game shows it after the first one.
    #[serde(skip)]
    activity: BTreeMap<CompanyID, WarehouseActivity>,
    // Saved as its own `cargo_progress` entry so the book's encoding is unchanged.
    #[serde(skip)]
    pub(crate) progress: BTreeMap<LoadId, LoadProgress>,
    // Set by a road edit and taken by the next update, within the same tick.
    #[serde(skip)]
    topology_edited: bool,
    #[serde(skip)]
    no_import_route: BTreeSet<Endpoint>,
}

impl Default for CargoBook {
    fn default() -> Self {
        Self {
            next_id: 0,
            shipments: BTreeMap::new(),
            loads: BTreeMap::new(),
            policies: BTreeMap::new(),
            restocking: BTreeSet::new(),
            // New games truck zoned industry's output. Saves keep their setting.
            growable_freight: true,
            returning: BTreeMap::new(),
            suspended: BTreeMap::new(),
            days: BTreeMap::new(),
            imports: BTreeMap::new(),
            station_trucks: BTreeMap::new(),
            import_customers: BTreeMap::new(),
            routes: RouteCache::default(),
            activity: BTreeMap::new(),
            progress: BTreeMap::new(),
            topology_edited: false,
            no_import_route: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Stock {
    pub reserved: u32,
    pub incoming: u32,
}

type StockKey = (Endpoint, ItemID);

/// Why an unload was refused. The load is then cancelled with this reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryRejection {
    /// The token was already settled or cancelled.
    UnknownLoad,
    /// The driver, truck or destination differ from the manifest.
    WrongTrip,
    /// The driver never rode the loaded truck.
    TruckNotUsed,
    Expired,
    /// The source or destination was removed or changed owner.
    EndpointGone,
    DriverNotAtDestination,
    /// The driver no longer holds this load for the source company.
    AssignmentChanged,
    TruckNotParked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Release {
    /// A missed delivery: removal, route failure, expiry or a refused unload.
    Cancelled,
    /// A player policy change recalled the stock.
    Withdrawn,
}

impl CargoBook {
    fn next_token(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn manifest_for(&self, vehicle: VehicleID) -> Option<&TruckLoad> {
        self.loads.values().find(|load| load.vehicle == vehicle)
    }

    /// Delivery outcomes and per-recipient receipts for an elapsed budget day.
    pub fn freight_day(&self, elapsed_day: u64) -> Option<&FreightDay> {
        self.days.get(&elapsed_day)
    }

    fn day_mut(&mut self, now: u64) -> &mut FreightDay {
        let day = now / TICKS_PER_BUDGET_DAY;
        if !self.days.contains_key(&day) {
            self.days.retain(|&kept, _| kept + FREIGHT_DAYS_KEPT > day);
        }
        self.days.entry(day).or_default()
    }

    pub fn stock_for(&self, building: BuildingID) -> BTreeMap<ItemID, Stock> {
        let mut stock = BTreeMap::<_, Stock>::new();
        for shipment in self.shipments.values() {
            let ItemQuantity { item, units } = shipment.quantity;
            if shipment.source.building == building {
                stock.entry(item).or_default().reserved += units;
            }
            if shipment.destination.building == building {
                stock.entry(item).or_default().incoming += units;
            }
        }
        for load in self
            .loads
            .values()
            .filter(|load| load.destination() == building)
        {
            for &ItemQuantity { item, units } in &load.items {
                stock.entry(item).or_default().incoming += units;
            }
        }
        stock
    }

    pub fn incoming_loads(&self, building: BuildingID) -> impl Iterator<Item = &TruckLoad> {
        self.loads
            .values()
            .filter(move |load| load.destination.building == building)
    }

    pub fn warehouse_policy(&self, company: CompanyID, storage: &WarehouseSpec) -> WarehousePolicy {
        self.policies
            .get(&company)
            .copied()
            .unwrap_or(storage.default_policy)
    }

    /// Units of shared warehouse space in use: on hand, queued or driving in,
    /// and outbound stock that returns if its trip is cancelled.
    pub fn warehouse_committed(
        &self,
        market: &Market,
        owner: SoulID,
        building: BuildingID,
        storage: &WarehouseSpec,
    ) -> u32 {
        let on_hand: u32 = stored_items(storage)
            .into_iter()
            .map(|item| market.capital(owner, item).max(0) as u32)
            .sum();
        let incoming: u32 = self
            .stock_for(building)
            .values()
            .map(|stock| stock.incoming + stock.reserved)
            .sum();
        let returnable: u32 = self.outgoing_loads(building).map(TruckLoad::units).sum();
        on_hand + incoming + returnable
    }

    /// Whether the warehouse is below its low watermark and refilling to high.
    pub fn is_restocking(&self, company: CompanyID) -> bool {
        self.restocking.contains(&company)
    }

    /// Recipients and items with a loaded truck on its way and not yet overdue,
    /// or an open import.
    pub(crate) fn inputs_in_transit(&self, now: u64) -> BTreeSet<(SoulID, ItemID)> {
        let loads = self
            .loads
            .values()
            .filter(|load| now < load.due_at)
            .flat_map(|load| {
                load.items
                    .iter()
                    .map(|line| (load.destination.soul, line.item))
            });
        let orders = self.imports.values().flat_map(|desk| {
            desk.orders
                .iter()
                .map(|order| (order.buyer.soul, order.quantity.item))
        });
        let trips = self
            .station_trucks
            .values()
            .filter_map(|truck| truck.trip.as_ref())
            .flat_map(|trip| trip.items.iter().map(|line| (trip.buyer.soul, line.item)));
        loads.chain(orders).chain(trips).collect()
    }

    /// Units imported for `(buyer, item)` that are on order or on a station truck.
    fn imports_pending(&self) -> BTreeMap<(Endpoint, ItemID), u64> {
        let mut units = BTreeMap::new();
        for order in self.imports.values().flat_map(|desk| &desk.orders) {
            *units.entry((order.buyer, order.quantity.item)).or_default() +=
                u64::from(order.quantity.units);
        }
        for trip in self
            .station_trucks
            .values()
            .filter_map(|truck| truck.trip.as_ref())
        {
            for line in &trip.items {
                *units.entry((trip.buyer, line.item)).or_default() += u64::from(line.units);
            }
        }
        units
    }

    /// A train call at the station: units on order by rail join its stock, up
    /// to `capacity`. Returns the remaining rail backlog capped for dispatch.
    pub(crate) fn unload_train(&mut self, station: SoulID, capacity: u32) -> u32 {
        let Some(desk) = self
            .imports
            .iter_mut()
            .find_map(|(end, desk)| (end.soul == station).then_some(desk))
        else {
            return 0;
        };
        let mut room = capacity;
        for (&item, units) in &mut desk.on_rail {
            let unloaded = bounded_transfer(*units, room);
            *units -= u64::from(unloaded);
            room -= unloaded;
            *desk.stock.entry(item).or_default() += u64::from(unloaded);
            if room == 0 {
                break;
            }
        }
        desk.on_rail.retain(|_, units| *units > 0);
        desk.stock.retain(|_, units| *units > 0);
        desk.wanted_cargo()
    }

    /// Import stock waiting at a station for its trucks.
    pub fn import_stock(&self, station: BuildingID) -> BTreeMap<ItemID, u64> {
        self.imports
            .iter()
            .find_map(|(end, desk)| (end.building == station).then(|| desk.stock.clone()))
            .unwrap_or_default()
    }

    /// Why a company's input is not here yet, if imports are involved.
    pub fn import_status(&self, company: SoulID) -> Option<ImportStatus> {
        let ordered = self
            .imports
            .values()
            .flat_map(|desk| &desk.orders)
            .filter(|order| order.buyer.soul == company)
            .map(|order| order.ordered_at);
        let carried = self
            .station_trucks
            .values()
            .filter_map(|truck| truck.trip.as_ref())
            .filter(|trip| trip.buyer.soul == company && !trip.items.is_empty())
            .map(|trip| trip.ordered_at);
        if let Some(since) = ordered.chain(carried).min() {
            return Some(ImportStatus::Waiting { since });
        }
        self.no_import_route
            .iter()
            .any(|end| end.soul == company)
            .then_some(ImportStatus::NoRoute)
    }

    /// Companies with no import route, and those waiting too long for one.
    pub fn import_problems(&self, now: u64) -> ImportProblems {
        let mut slow = BTreeSet::new();
        for order in self.imports.values().flat_map(|desk| &desk.orders) {
            if now >= order.ordered_at + IMPORT_WAIT_WARNING {
                slow.insert(order.buyer.building);
            }
        }
        for trip in self
            .station_trucks
            .values()
            .filter_map(|truck| truck.trip.as_ref())
        {
            if !trip.items.is_empty() && now >= trip.ordered_at + IMPORT_WAIT_WARNING {
                slow.insert(trip.buyer.building);
            }
        }
        ImportProblems {
            no_route: self
                .no_import_route
                .iter()
                .map(|end| end.building)
                .collect(),
            slow: slow.into_iter().collect(),
        }
    }

    /// The warehouse's state at the last freight match, if it has been matched.
    pub fn warehouse_activity(&self, company: CompanyID) -> Option<WarehouseActivity> {
        self.activity.get(&company).copied()
    }

    /// Stores a warehouse policy. Queued exports return to the shelf and are
    /// matched again under the new policy. Reserve locally also recalls
    /// exports that are already on the road. Neither counts as a missed delivery.
    pub(crate) fn set_warehouse_policy(
        sim: &mut Simulation,
        company: CompanyID,
        policy: WarehousePolicy,
    ) {
        let (world, resources) = sim.world_res();
        let mut book = resources.write::<CargoBook>();
        // Compare with the policy in effect, which may be the prototype default.
        let current = world
            .companies
            .get(company)
            .and_then(|entity| entity.comp.proto.prototype().storage.as_ref())
            .map(|storage| book.warehouse_policy(company, storage));
        book.policies.insert(company, policy);
        if current == Some(policy) {
            return;
        }
        let exports = |source: Endpoint, dest: Endpoint| {
            source.soul == SoulID::GoodsCompany(company)
                && matches!(dest.soul, SoulID::FreightStation(_))
        };
        let shipments = book
            .shipments
            .iter()
            .filter_map(|(&id, shipment)| {
                exports(shipment.source, shipment.destination).then_some(id)
            })
            .collect();
        let loads = book
            .loads
            .iter()
            .filter(|_| policy == WarehousePolicy::ReserveLocally)
            .filter_map(|(&id, load)| exports(load.source, load.destination).then_some(id))
            .collect();
        release(
            &mut book,
            world,
            resources,
            shipments,
            loads,
            Release::Withdrawn,
        );
    }

    pub(crate) fn forget_company(&mut self, company: CompanyID) {
        self.policies.remove(&company);
        self.restocking.remove(&company);
        self.activity.remove(&company);
    }

    pub fn outgoing_loads(&self, building: BuildingID) -> impl Iterator<Item = &TruckLoad> {
        self.loads
            .values()
            .filter(move |load| load.source.building == building)
    }

    fn queued(&self) -> BTreeMap<StockKey, u32> {
        let mut units = BTreeMap::new();
        for shipment in self.shipments.values() {
            *units
                .entry((shipment.destination, shipment.quantity.item))
                .or_default() += shipment.quantity.units;
        }
        units
    }

    fn in_transit(&self) -> BTreeMap<StockKey, u32> {
        self.arriving(None)
    }

    /// Units on loaded trucks, only those not yet overdue at `now` if given.
    fn arriving(&self, now: Option<u64>) -> BTreeMap<StockKey, u32> {
        let mut units = BTreeMap::new();
        for load in self
            .loads
            .values()
            .filter(|load| now.is_none_or(|now| now < load.due_at))
        {
            for line in &load.items {
                *units.entry((load.destination, line.item)).or_default() += line.units;
            }
        }
        units
    }

    fn reserve(&mut self, market: &mut Market, shipment: Shipment) {
        let id = ShipmentId(self.next_token());
        let ItemQuantity { item, units } = shipment.quantity;
        market.produce(shipment.source.soul, item, -(units as i32));
        self.shipments.insert(id, shipment);
    }

    /// A road route that matching may use: both ends reach the same driving
    /// component from their parking, and the route is not suspended.
    fn physical_route(&mut self, map: &Map, from: Endpoint, to: Endpoint, now: u64) -> bool {
        !self.is_suspended(map, from, to, now)
            && self.routes.reaches(map, from.building, to.building)
    }

    /// A reachable route whose outbound leg gave up and is cooling down.
    fn held_route(&mut self, map: &Map, from: Endpoint, to: Endpoint, now: u64) -> bool {
        self.is_suspended(map, from, to, now)
            && self.routes.reaches(map, from.building, to.building)
    }

    fn is_suspended(&self, map: &Map, from: Endpoint, to: Endpoint, now: u64) -> bool {
        self.suspended
            .get(&(from, to))
            .is_some_and(|route| now < route.until && route.roads == connected_roads(map, from, to))
    }

    fn suspend_route(&mut self, map: &Map, from: Endpoint, to: Endpoint, now: u64) {
        self.suspended.insert(
            (from, to),
            SuspendedRoute {
                until: now + FAILED_ROUTE_COOLDOWN,
                topology: 0,
                roads: connected_roads(map, from, to),
            },
        );
    }
}

/// A road edit may reopen any route. Suspended routes are matched again, and
/// queued shipments are checked on this tick's update instead of the next match.
pub(crate) fn on_topology_changed(sim: &Simulation) {
    let mut book = sim.write::<CargoBook>();
    book.suspended.clear();
    book.topology_edited = true;
}

fn connected_roads(map: &Map, from: Endpoint, to: Endpoint) -> [Option<RoadID>; 2] {
    [from, to].map(|end| map.get(end.building).and_then(|b| b.connected_road))
}

#[derive(Clone, Copy)]
struct DispatchPolicy {
    capacity: u32,
    minimum: u32,
    max_wait: u64,
}

impl DispatchPolicy {
    fn for_prototype(proto: &GoodsCompanyPrototype) -> Option<Self> {
        if proto.n_trucks == 0 {
            return None;
        }
        let capacity = proto.cargo_capacity_units?;
        Some(Self {
            capacity,
            minimum: proto.dispatch_min_units.unwrap_or(capacity),
            max_wait: u64::from(
                proto
                    .dispatch_max_wait_ticks
                    .unwrap_or((2 * TICKS_PER_HOUR) as u32),
            ),
        })
    }

    fn quantity(self, reserved: u32, free: u32, age: u64, urgent: bool) -> u32 {
        let units = reserved.min(free).min(self.capacity);
        if urgent || age >= self.max_wait || units >= self.minimum.min(free) {
            units
        } else {
            0
        }
    }
}

pub(crate) fn uses_road_freight(
    proto: &GoodsCompanyPrototype,
    item: ItemID,
    growable_freight: bool,
) -> bool {
    (growable_freight || !proto.id.is_growable())
        && DispatchPolicy::for_prototype(proto).is_some()
        && !household_items().contains(&item)
}

/// Where a truck parks to serve a building, and the driving component it joins.
#[derive(Clone, Copy)]
struct ParkingAccess {
    component: usize,
    drive_pos: geom::Vec3,
}

/// Road reachability for freight, derived from the map and rebuilt when the
/// lane topology changes. Reachability compares the map's strongly connected
/// driving components, so reachability alone never runs a route search.
/// Driven lengths for warehouse placement and shift planning take one search
/// per ordered pair.
#[derive(Default)]
struct RouteCache {
    revision: (u64, u64),
    // Keyed by the building's road connection, which can change without a
    // lane topology change.
    access: BTreeMap<BuildingID, (Option<RoadID>, Option<ParkingAccess>)>,
    distances: BTreeMap<(BuildingID, BuildingID), Option<OrderedFloat<f32>>>,
    // Placing or removing the outside market does not change lane topology.
    rail_market: Option<BuildingID>,
    /// Stations whose platform has a rail route to the outside.
    rail: BTreeMap<BuildingID, bool>,
    searches: u64,
}

impl RouteCache {
    fn refresh(&mut self, map: &Map) {
        let revision = (map.identity(), map.walking_topology_generation);
        if self.revision != revision {
            *self = Self {
                revision,
                searches: self.searches,
                ..Self::default()
            };
        }
        let rail_market = map.external_train_stations.first().copied();
        if self.rail_market != rail_market {
            self.rail_market = rail_market;
            self.rail.clear();
        }
    }

    fn forget(&mut self, building: BuildingID) {
        self.access.remove(&building);
        self.distances
            .retain(|&(from, to), _| from != building && to != building);
    }

    /// Mirrors the truck router: park near the door on a driving lane that is
    /// connected both ways with the building's own road.
    fn access(&mut self, map: &Map, id: BuildingID) -> Option<ParkingAccess> {
        self.refresh(map);
        let Some(building) = map.get(id) else {
            self.forget(id);
            return None;
        };
        let road = building.connected_road;
        if let Some(&(cached_road, access)) = self.access.get(&id) {
            if cached_road == road {
                return access;
            }
            self.forget(id);
        }
        let access = (|| {
            let connected = map.get(road?)?;
            let spot = ParkingManagement::default()
                .reserve_near(building.door_pos, map)
                .ok()?;
            let drive_pos = spot.park_pos(map)?;
            let lane = map.nearest_lane(drive_pos, LaneKind::Driving, None)?;
            let component = map.driving_component(lane)?;
            connected
                .lanes_iter()
                .any(|(lane, _)| map.driving_component(lane) == Some(component))
                .then_some(ParkingAccess {
                    component,
                    drive_pos,
                })
        })();
        self.access.insert(id, (road, access));
        access
    }

    /// Whether a train can run from the outside to the station's platform track and back.
    fn rail_served(&mut self, map: &Map, station: BuildingID, stop: geom::Vec3) -> bool {
        self.refresh(map);
        *self.rail.entry(station).or_insert_with(|| {
            let Some(outside) = map
                .external_train_stations
                .first()
                .and_then(|&id| map.get(id))
            else {
                return false;
            };
            // The train dispatcher looks for the platform track this close to the stop.
            let Some(platform) = map
                .nearest_lane(stop, LaneKind::Rail, Some(50.0))
                .and_then(|id| map.get(id))
            else {
                return false;
            };
            let stop = platform.points.project(stop);
            let outside = outside.obb.center().z(0.0);
            [(outside, stop), (stop, outside)]
                .into_iter()
                .all(|(from, to)| {
                    Itinerary::route(Tick(0), from, to, map, PathKind::Rail).is_some()
                })
        })
    }

    fn reaches(&mut self, map: &Map, from: BuildingID, to: BuildingID) -> bool {
        from != to
            && self
                .access(map, from)
                .zip(self.access(map, to))
                .is_some_and(|(from, to)| from.component == to.component)
    }

    fn distance(
        &mut self,
        map: &Map,
        from: BuildingID,
        to: BuildingID,
    ) -> Option<OrderedFloat<f32>> {
        if !self.reaches(map, from, to) {
            return None;
        }
        if let Some(&distance) = self.distances.get(&(from, to)) {
            return distance;
        }
        let (start, end) = (self.access[&from].1?, self.access[&to].1?);
        self.searches += 1;
        let distance = vehicle_distance(map, start.drive_pos, end.drive_pos);
        self.distances.insert((from, to), distance);
        distance
    }
}

fn vehicle_distance(map: &Map, start: geom::Vec3, end: geom::Vec3) -> Option<OrderedFloat<f32>> {
    let itinerary = Itinerary::route(Tick(0), start, end, map, PathKind::Vehicle)?;
    let local = itinerary.local_path();
    let mut length = local.last().map_or(0.0, |point| start.distance(*point))
        + local
            .windows(2)
            .map(|pair| pair[0].distance(pair[1]))
            .sum::<f32>();
    let route = itinerary.get_route()?;
    if let Some((last, intermediate)) = route.reversed_route.split_first() {
        length += intermediate
            .iter()
            .filter_map(|turn| turn.raw_points(map))
            .map(|points| points.length())
            .sum::<f32>();
        let points = last.raw_points(map)?;
        let (projected, next_point) = points.project_segment(end);
        let segment = next_point.saturating_sub(1);
        length += points.as_slice()[..=segment]
            .windows(2)
            .map(|pair| pair[0].distance(pair[1]))
            .sum::<f32>();
        length += points.as_slice()[segment].distance(projected) + projected.distance(end);
    }
    Some(OrderedFloat(length))
}

#[derive(Clone, Copy)]
struct Input {
    destination: Endpoint,
    amount: u32,
    recipe_ticks: u64,
}

impl Input {
    fn new(destination: Endpoint, amount: i32, full_speed_batch_ticks: f64) -> Self {
        Self {
            destination,
            amount: amount as u32,
            recipe_ticks: full_speed_batch_ticks.ceil() as u64,
        }
    }

    fn target(self, policy: DispatchPolicy) -> u32 {
        // Recipes run around the clock. Carry stock through the gap in
        // driver coverage as well as through batching and travel.
        let during_lead = (BETWEEN_DRIVER_SHIFTS + policy.max_wait + DELIVERY_LEAD)
            .saturating_mul(u64::from(self.amount))
            .div_ceil(self.recipe_ticks.max(1));
        policy
            .capacity
            .saturating_add(during_lead.min(u64::from(u32::MAX)) as u32)
            .saturating_add(self.amount)
            .min(i32::MAX as u32)
    }

    fn storage_limit(self, policy: DispatchPolicy) -> u32 {
        // Leave unloading room above the reorder level so a small stock top-up
        // waits for more cargo instead of appearing to fill the destination.
        self.target(policy)
            .saturating_add(policy.capacity)
            .min(i32::MAX as u32)
    }
}

#[cfg(test)]
std::thread_local! {
    static TEST_BATCH_TICKS_SCALE: std::cell::Cell<f64> = const { std::cell::Cell::new(1.0) };
}

fn freight_batch_ticks(business: ActiveBusiness) -> Option<f64> {
    let ticks = business.rates().full_speed_batch_ticks;
    #[cfg(test)]
    let ticks = ticks.map(|ticks| ticks * TEST_BATCH_TICKS_SCALE.get());
    ticks
}

fn on_hand(market: &Market, endpoint: Endpoint, item: ItemID) -> u32 {
    market.capital(endpoint.soul, item).max(0) as u32
}

fn driver_truck(
    world: &World,
    source: Endpoint,
    company: CompanyID,
    driver: HumanID,
) -> Option<VehicleID> {
    let company = world.companies.get(company)?;
    let work = world.humans.get(driver)?.work.as_ref()?;
    let WorkKind::Driver { truck, .. } = work.kind else {
        return None;
    };
    (work.workplace == source.building
        && company.workers.0.contains(&driver)
        && company.comp.trucks.contains(&truck)
        && world
            .vehicles
            .get(truck)
            .is_some_and(|v| matches!(v.vehicle.kind, VehicleKind::Truck)))
    .then_some(truck)
}

fn assigned_drivers(
    world: &World,
    source: Endpoint,
    company: CompanyID,
) -> impl Iterator<Item = (HumanID, VehicleID)> + '_ {
    let entity = &world.companies[company];
    let proto = entity.comp.proto.prototype();
    let drivers = if proto.storage.is_some() || proto.n_trucks > 1 {
        entity.workers.0.as_slice()
    } else {
        entity.comp.driver.as_slice()
    };
    drivers
        .iter()
        .filter_map(move |&driver| Some((driver, driver_truck(world, source, company, driver)?)))
}

fn assigned_driver(
    world: &World,
    source: Endpoint,
    company: CompanyID,
) -> Option<(HumanID, VehicleID)> {
    assigned_drivers(world, source, company).next()
}

fn has_fleet(world: &World, company: CompanyID) -> bool {
    world.companies[company]
        .comp
        .trucks
        .iter()
        .any(|&truck| world.vehicles.contains_key(truck))
}

fn owns_assignment(
    world: &World,
    source: Endpoint,
    company: CompanyID,
    driver: HumanID,
    truck: VehicleID,
) -> bool {
    world.companies.get(company).is_some_and(|entity| {
        let proto = entity.comp.proto.prototype();
        (entity.comp.driver == Some(driver) || proto.storage.is_some() || proto.n_trucks > 1)
            && driver_truck(world, source, company, driver) == Some(truck)
    })
}

/// Physical items a warehouse can hold, without duplicates. Household items
/// never travel by road freight.
pub fn stored_items(spec: &WarehouseSpec) -> BTreeSet<ItemID> {
    spec.accepted_items
        .iter()
        .copied()
        .filter(|item| !household_items().contains(item))
        .collect()
}

struct Warehouse {
    id: CompanyID,
    endpoint: Endpoint,
    spec: &'static WarehouseSpec,
    items: BTreeSet<ItemID>,
    policy: WarehousePolicy,
    dispatch: DispatchPolicy,
    staffed: bool,
    on_hand: u32,
    /// On-hand plus incoming units, in total and per item.
    staged: u32,
    staged_items: BTreeMap<ItemID, u32>,
    /// Staged units plus outbound units that may return after a cancellation.
    committed: u32,
}

fn straight_distance(map: &Map, from: Endpoint, to: Endpoint) -> (OrderedFloat<f32>, Endpoint) {
    (
        OrderedFloat(
            map.buildings()[from.building]
                .door_pos
                .distance2(map.buildings()[to.building].door_pos),
        ),
        to,
    )
}

fn driving_ticks(metres: f32) -> u64 {
    (metres / FREIGHT_PACE_METERS_PER_SECOND * TICKS_PER_REALTIME_SECOND as f32).ceil() as u64
}

fn round_trip_ticks(
    routes: &mut RouteCache,
    map: &Map,
    source: Endpoint,
    dest: Endpoint,
) -> Option<u64> {
    let outbound = routes.distance(map, source.building, dest.building)?;
    let inbound = routes.distance(map, dest.building, source.building)?;
    Some(driving_ticks(outbound.0 + inbound.0) + HANDLING_TICKS)
}

struct DriverWindow {
    ticks: u64,
    departures: u32,
}

impl DriverWindow {
    // Start with the current shift, or the next one if the driver is off duty.
    // Ten hours also bounds the planning horizon for an always-active roster.
    fn new(work: &crate::souls::desire::Work, time: &GameTime, busy_ticks: u64) -> Self {
        let wait = work.work_inter.dist_start(&time.daytime);
        let start = time.daytime.gamesec() + wait;
        let (mut fits, mut limit) = (0, 10 * SECONDS_PER_HOUR + 1);
        while limit - fits > 1 {
            let middle = (fits + limit) / 2;
            if work.work_inter.is_active(&DayTime::new(start + middle)) {
                fits = middle;
            } else {
                limit = middle;
            }
        }
        Self {
            ticks: game_secs_to_ticks(fits as u64)
                .saturating_sub(busy_ticks.saturating_sub(game_secs_to_ticks(wait as u64))),
            departures: DEPARTURES_PER_DRIVER,
        }
    }
}

struct DepartureQueue {
    capacity: u32,
    windows: Vec<DriverWindow>,
    queued: BTreeMap<Endpoint, u32>,
    fits: BTreeSet<Endpoint>,
}

impl DepartureQueue {
    fn has_room(&self, destination: Endpoint, urgent: bool) -> bool {
        let queued = self.queued.get(&destination).copied().unwrap_or(0);
        if urgent && queued >= self.capacity {
            return false;
        }
        (queued % self.capacity != 0 && self.fits.contains(&destination))
            || self
                .windows
                .iter()
                .any(|window| window.departures > 0 && window.ticks >= HANDLING_TICKS)
    }

    fn plan(&mut self, ticks: u64) -> bool {
        let Some(window) = self
            .windows
            .iter_mut()
            .filter(|window| window.departures > 0 && window.ticks >= ticks)
            .min_by_key(|window| window.ticks)
        else {
            return false;
        };
        window.ticks -= ticks;
        window.departures -= 1;
        true
    }

    fn account_for_queued(&mut self, destination: Endpoint, ticks: u64) {
        let mut fits = true;
        for _ in 0..self.queued[&destination].div_ceil(self.capacity) {
            if !self.plan(ticks) {
                fits = false;
                // Previously staged stock still owns a slot if the shift is
                // ending or the save predates the bounded queue.
                if let Some(window) = self
                    .windows
                    .iter_mut()
                    .filter(|window| window.departures > 0)
                    .max_by_key(|window| window.ticks)
                {
                    window.departures -= 1;
                    window.ticks = 0;
                    continue;
                }
                return;
            }
        }
        if fits {
            self.fits.insert(destination);
        }
    }

    fn reserve(&mut self, destination: Endpoint, wanted: u32, ticks: u64, urgent: bool) -> u32 {
        let queued = self.queued.get(&destination).copied().unwrap_or(0);
        let wanted = if urgent {
            wanted.min(self.capacity.saturating_sub(queued))
        } else {
            wanted
        };
        let mut units = 0;
        if self.fits.contains(&destination) {
            let partial = queued % self.capacity;
            if partial > 0 {
                units = wanted.min(self.capacity - partial);
            }
        }
        while units < wanted && self.plan(ticks) {
            units += (wanted - units).min(self.capacity);
        }
        if units > 0 {
            self.queued.insert(destination, queued + units);
            self.fits.insert(destination);
        }
        units
    }
}

struct Matching<'a> {
    book: &'a mut CargoBook,
    market: &'a mut Market,
    map: &'a Map,
    queued: BTreeMap<StockKey, u32>,
    in_transit: BTreeMap<StockKey, u32>,
    outbound: BTreeSet<StockKey>,
    departures: BTreeMap<Endpoint, DepartureQueue>,
    now: u64,
}

impl Matching<'_> {
    fn queue(
        &mut self,
        source: Endpoint,
        destination: Endpoint,
        item: ItemID,
        wanted: u32,
        urgent: bool,
    ) -> u32 {
        if wanted == 0 {
            return 0;
        }
        let queue = self.departures.get_mut(&source).unwrap();
        if !queue.has_room(destination, urgent) {
            return 0;
        }
        let Some(ticks) = round_trip_ticks(&mut self.book.routes, self.map, source, destination)
        else {
            return 0;
        };
        let units = queue.reserve(destination, wanted, ticks, urgent);
        if units == 0 {
            return 0;
        }
        self.book.reserve(
            self.market,
            Shipment {
                source,
                destination,
                quantity: ItemQuantity { item, units },
                reserved_at: self.now,
            },
        );
        *self.queued.entry((destination, item)).or_default() += units;
        units
    }

    fn incoming(&self, key: StockKey) -> u32 {
        self.queued.get(&key).copied().unwrap_or(0)
            + self.in_transit.get(&key).copied().unwrap_or(0)
    }

    /// The recipe cannot run even after the stock already on its way arrives.
    fn short(&self, input: Input, item: ItemID) -> bool {
        on_hand(self.market, input.destination, item) + self.incoming((input.destination, item))
            < input.amount
    }

    fn deficit(&self, policy: DispatchPolicy, input: Input, item: ItemID) -> u32 {
        let stock = on_hand(self.market, input.destination, item);
        input
            .target(policy)
            .saturating_sub(stock.saturating_add(self.incoming((input.destination, item))))
    }

    /// Units of `available` a reorder for the input would take now.
    fn wanted(&self, policy: DispatchPolicy, input: Input, item: ItemID, available: u32) -> u32 {
        let key = (input.destination, item);
        let pending = self.queued.get(&key).copied().unwrap_or(0);
        let stock = on_hand(self.market, input.destination, item);
        let deficit = self.deficit(policy, input, item);
        // Allow smaller orders when some stock is queued or the recipe cannot run.
        if pending == 0 && deficit < policy.minimum && stock >= input.amount {
            return 0;
        }
        deficit.min(available)
    }

    fn reserve_for_input(
        &mut self,
        policy: DispatchPolicy,
        source: Endpoint,
        input: Input,
        item: ItemID,
        available: u32,
        urgent: bool,
    ) -> u32 {
        let units = self.wanted(policy, input, item, available);
        self.queue(source, input.destination, item, units, urgent)
    }

    fn route(&mut self, from: Endpoint, to: Endpoint) -> bool {
        self.book.physical_route(self.map, from, to, self.now)
    }

    /// A suspended route that still reaches its consumer. Its reorder stays
    /// on hand, so a failure that may be passing does not export it.
    fn held_route(&mut self, from: Endpoint, to: Endpoint) -> bool {
        self.book.held_route(self.map, from, to, self.now)
    }

    fn road_distance(&mut self, from: Endpoint, to: Endpoint) -> Option<OrderedFloat<f32>> {
        if !self.route(from, to) {
            return None;
        }
        self.book
            .routes
            .distance(self.map, from.building, to.building)
    }

    fn nearest_station(&mut self, from: Endpoint, stations: &[Endpoint]) -> Option<Endpoint> {
        stations
            .iter()
            .copied()
            .filter_map(|station| {
                self.road_distance(from, station)
                    .map(|distance| (distance, station))
            })
            .min()
            .map(|(_, station)| station)
    }

    /// A staffed producer at least as close as the warehouse already holds
    /// enough stock to cover the recipe's reorder.
    fn direct_supply_covers(
        &mut self,
        producers: &[Endpoint],
        policy: DispatchPolicy,
        input: Input,
        item: ItemID,
        warehouse_distance: OrderedFloat<f32>,
    ) -> bool {
        let destination = input.destination;
        if on_hand(self.market, destination, item) < input.amount {
            return false;
        }
        let deficit = self.deficit(policy, input, item).max(1);
        producers.iter().any(|&producer| {
            on_hand(self.market, producer, item) >= deficit
                && self
                    .road_distance(producer, destination)
                    .is_some_and(|distance| distance <= warehouse_distance)
        })
    }

    fn warehouse_output(
        &mut self,
        warehouse: &mut Warehouse,
        inputs: &BTreeMap<ItemID, Vec<Input>>,
        staffed_producers: &BTreeMap<ItemID, Vec<Endpoint>>,
        stations: &[Endpoint],
        urgent: bool,
    ) {
        if !warehouse.staffed {
            return;
        }
        let source = warehouse.endpoint;
        let policy = warehouse.dispatch;
        let before: BTreeMap<_, _> = warehouse
            .items
            .iter()
            .map(|&item| (item, on_hand(self.market, source, item)))
            .filter(|&(_, units)| units > 0)
            .collect();
        let mut stock = before.clone();

        // Local recipes claim stock for every item before anything is exported.
        // Open routes are served first, then suspended ones keep their reorder on site.
        let mut used_locally = BTreeSet::new();
        let mut kept = BTreeMap::<ItemID, u32>::new();
        for (&item, available) in &mut stock {
            let mut candidates = inputs.get(&item).cloned().unwrap_or_default();
            candidates.sort_unstable_by_key(|input| {
                (
                    on_hand(self.market, input.destination, item) >= input.amount,
                    straight_distance(self.map, source, input.destination),
                )
            });
            let mut held = Vec::new();
            for input in candidates {
                if urgent && !self.short(input, item) {
                    continue;
                }
                let Some(distance) = self.road_distance(source, input.destination) else {
                    if !urgent && self.held_route(source, input.destination) {
                        used_locally.insert(item);
                        held.push(input);
                    }
                    continue;
                };
                used_locally.insert(item);
                if *available == 0 {
                    break;
                }
                let producers = staffed_producers.get(&item).map_or(&[][..], Vec::as_slice);
                if self.direct_supply_covers(producers, policy, input, item, distance) {
                    continue;
                }
                *available -=
                    self.reserve_for_input(policy, source, input, item, *available, urgent);
            }
            for input in held {
                let units = self.wanted(policy, input, item, *available);
                *available -= units;
                *kept.entry(item).or_default() += units;
            }
        }

        // The watermarks apply to the shared space. Items no reachable recipe
        // uses are surplus down to the low watermark and leave first. Items
        // local recipes use leave only from above the high watermark.
        if !urgent && warehouse.policy != WarehousePolicy::ReserveLocally {
            let held: u32 = stock.values().sum();
            let mut above_low = held.saturating_sub(warehouse.spec.low_water_units);
            let mut above_high = held.saturating_sub(warehouse.spec.high_water_units);
            let station = (above_low > 0)
                .then(|| self.nearest_station(source, stations))
                .flatten();
            for local in [false, true] {
                let Some(station) = station else {
                    break;
                };
                for (&item, available) in &mut stock {
                    if used_locally.contains(&item) != local || item.prototype().optout_exttrade {
                        continue;
                    }
                    let budget = if local { above_high } else { above_low };
                    let units = (*available).min(budget).min(policy.capacity);
                    // Keep at most one unsent export batch per item.
                    if units == 0
                        || self.book.shipments.values().any(|s| {
                            s.source == source
                                && s.destination == station
                                && s.quantity.item == item
                        })
                    {
                        continue;
                    }
                    let units = self.queue(source, station, item, units, false);
                    *available -= units;
                    above_low -= units;
                    above_high = above_high.saturating_sub(units);
                }
            }
        }

        // Stock kept on site for a suspended route has not left.
        for (item, units) in before {
            let sent = units - stock[&item] - kept.get(&item).copied().unwrap_or(0);
            if sent > 0 {
                warehouse.on_hand -= sent;
                warehouse.staged -= sent;
                *warehouse.staged_items.entry(item).or_default() -= sent;
                self.outbound.insert((source, item));
            }
        }
    }

    fn warehouse_input(
        &mut self,
        source: Endpoint,
        item: ItemID,
        inputs: &[Input],
        warehouses: &mut [Warehouse],
        stations: &[Endpoint],
        mut available: u32,
    ) -> u32 {
        // An unstaffed warehouse could not pass the stock on.
        let mut candidates: Vec<_> = warehouses
            .iter_mut()
            .filter(|warehouse| {
                warehouse.staffed
                    && warehouse.items.contains(&item)
                    && !self.outbound.contains(&(warehouse.endpoint, item))
            })
            .collect();
        candidates.sort_unstable_by_key(|warehouse| {
            straight_distance(self.map, source, warehouse.endpoint)
        });
        for warehouse in candidates {
            let dest = warehouse.endpoint;
            let room = warehouse
                .spec
                .capacity_units
                .saturating_sub(warehouse.committed);
            if room == 0 {
                continue;
            }
            let Some(inbound) = self.road_distance(source, dest) else {
                continue;
            };
            // The warehouse helps a reachable recipe that is short or closer to it.
            let mut serves_local = false;
            for &input in inputs {
                let consumer = input.destination;
                let Some(local) = self.road_distance(dest, consumer) else {
                    continue;
                };
                if self.short(input, item)
                    || self
                        .road_distance(source, consumer)
                        .is_none_or(|direct| local < direct)
                {
                    serves_local = true;
                    break;
                }
            }
            let surplus = warehouse.policy == WarehousePolicy::ExportSurplus
                && !item.prototype().optout_exttrade
                && self.nearest_station(dest, stations).is_some_and(|station| {
                    let direct = self
                        .road_distance(source, station)
                        .unwrap_or(OrderedFloat(f32::INFINITY));
                    inbound < direct
                        && self
                            .road_distance(dest, station)
                            .is_some_and(|onward| onward < direct)
                });
            let missing = warehouse.staged_items.get(&item).copied().unwrap_or(0) == 0;
            let limit = if surplus {
                warehouse.spec.capacity_units
            } else if serves_local && self.book.restocking.contains(&warehouse.id) {
                warehouse.spec.high_water_units
            } else if serves_local && missing {
                // Space above the watermark still takes one load of a locally
                // used item the warehouse holds none of.
                warehouse.staged + warehouse.dispatch.capacity
            } else {
                continue;
            };
            let wanted = available
                .min(limit.saturating_sub(warehouse.staged))
                .min(room);
            let units = self.queue(source, dest, item, wanted, false);
            if units == 0 {
                continue;
            }
            warehouse.staged += units;
            *warehouse.staged_items.entry(item).or_default() += units;
            warehouse.committed += units;
            available -= units;
            if warehouse.staged >= warehouse.spec.high_water_units {
                self.book.restocking.remove(&warehouse.id);
            }
            if available == 0 {
                break;
            }
        }
        available
    }
}

fn departure_queues(
    book: &mut CargoBook,
    world: &World,
    map: &Map,
    time: &GameTime,
    now: u64,
    sources: impl Iterator<Item = (CompanyID, Endpoint, DispatchPolicy)>,
) -> BTreeMap<Endpoint, DepartureQueue> {
    let mut queued = BTreeMap::<_, BTreeMap<_, u32>>::new();
    for shipment in book.shipments.values() {
        *queued
            .entry(shipment.source)
            .or_default()
            .entry(shipment.destination)
            .or_default() += shipment.quantity.units;
    }
    let loads: BTreeMap<_, _> = book
        .loads
        .values()
        .map(|load| (load.vehicle, load))
        .collect();
    sources
        .map(|(company, source, policy)| {
            let windows = assigned_drivers(world, source, company)
                .map(|(driver, truck)| {
                    let work = world.humans[driver].work.as_ref().unwrap();
                    let busy = if let Some(load) = loads.get(&truck) {
                        round_trip_ticks(&mut book.routes, map, source, load.destination)
                            .unwrap_or(DELIVERY_LEAD)
                            .saturating_sub(now.saturating_sub(load.departed_at))
                    } else if book.returning.contains_key(&truck) {
                        let door = map.buildings()[source.building].door_pos;
                        driving_ticks(world.vehicles[truck].trans.pos.distance(door))
                            + HANDLING_TICKS
                    } else {
                        0
                    };
                    DriverWindow::new(work, time, busy)
                })
                .collect();
            let mut queue = DepartureQueue {
                capacity: policy.capacity,
                windows,
                queued: queued.remove(&source).unwrap_or_default(),
                fits: BTreeSet::new(),
            };
            let destinations: Vec<_> = queue.queued.keys().copied().collect();
            for destination in destinations {
                let ticks = round_trip_ticks(&mut book.routes, map, source, destination)
                    .unwrap_or(DELIVERY_LEAD);
                queue.account_for_queued(destination, ticks);
            }
            (source, queue)
        })
        .collect()
}

fn match_shipments(
    book: &mut CargoBook,
    world: &mut World,
    resources: &Resources,
    map: &Map,
    infos: &BuildingInfos,
    now: u64,
    released_sources: &BTreeSet<Endpoint>,
) {
    let sources: Vec<_> = world
        .companies
        .iter()
        .filter_map(|(id, company)| {
            let proto = company.comp.proto.prototype();
            if proto.storage.is_some() || (proto.id.is_growable() && !book.growable_freight) {
                return None;
            }
            let policy = DispatchPolicy::for_prototype(proto)?;
            let recipe = proto.recipe.as_ref()?;
            let source = Endpoint {
                soul: SoulID::GoodsCompany(id),
                building: company.comp.building,
            };
            (source.live(world, map, infos) && !released_sources.contains(&source))
                .then_some((id, source, policy, recipe))
        })
        .collect();
    let warehouse_sites: Vec<_> = world
        .companies
        .iter()
        .filter_map(|(id, company)| {
            let proto = company.comp.proto.prototype();
            let spec = proto.storage.as_ref()?;
            let dispatch = DispatchPolicy::for_prototype(proto)?;
            let endpoint = Endpoint {
                soul: SoulID::GoodsCompany(id),
                building: company.comp.building,
            };
            endpoint
                .live(world, map, infos)
                .then_some((id, endpoint, spec, dispatch))
        })
        .collect();
    if sources.is_empty() && warehouse_sites.is_empty() {
        return;
    }

    let items: BTreeSet<_> = sources
        .iter()
        .flat_map(|(_, _, _, recipe)| recipe.production.iter().map(|output| output.id))
        .chain(
            warehouse_sites
                .iter()
                .flat_map(|(_, _, spec, _)| stored_items(spec)),
        )
        .collect();
    let mut inputs = BTreeMap::<_, Vec<Input>>::new();
    for (id, company) in &world.companies {
        let proto = company.comp.proto.prototype();
        if proto.storage.is_some() {
            continue;
        }
        let Some(recipe) = proto.recipe.as_ref() else {
            continue;
        };
        let destination = Endpoint {
            soul: SoulID::GoodsCompany(id),
            building: company.comp.building,
        };
        let Some(batch_ticks) = map
            .get(destination.building)
            .and_then(|building| active_business(building, infos, world))
            .filter(|business| business.company == id)
            .and_then(freight_batch_ticks)
        else {
            continue;
        };
        for item in recipe
            .consumption
            .iter()
            .filter(|item| items.contains(&item.id))
        {
            inputs.entry(item.id).or_default().push(Input::new(
                destination,
                item.amount,
                batch_ticks,
            ));
        }
    }
    let stations: Vec<_> = world
        .freight_stations
        .iter()
        .filter_map(|(id, station)| {
            let endpoint = Endpoint {
                soul: SoulID::FreightStation(id),
                building: station.f.building,
            };
            endpoint.live(world, map, infos).then_some(endpoint)
        })
        .collect();
    let time = resources.read::<GameTime>();
    let departures = departure_queues(
        book,
        world,
        map,
        &time,
        now,
        sources
            .iter()
            .map(|&(id, source, policy, _)| (id, source, policy))
            .chain(
                warehouse_sites
                    .iter()
                    .map(|&(id, source, _, policy)| (id, source, policy)),
            ),
    );
    let mut market = resources.write::<Market>();
    let mut matching = Matching {
        queued: book.queued(),
        in_transit: book.in_transit(),
        book,
        market: &mut market,
        map,
        outbound: BTreeSet::new(),
        departures,
        now,
    };
    let mut warehouses = Vec::with_capacity(warehouse_sites.len());
    let mut staffed_producers = BTreeMap::<_, Vec<_>>::new();
    if !warehouse_sites.is_empty() {
        for &(id, source, _, recipe) in &sources {
            if assigned_driver(world, source, id).is_some() {
                for item in &recipe.production {
                    staffed_producers.entry(item.id).or_default().push(source);
                }
            }
        }
        for (id, endpoint, spec, dispatch) in warehouse_sites {
            let items = stored_items(spec);
            let mut on_hand_total = 0;
            let mut staged_items = BTreeMap::new();
            for &item in &items {
                let held = on_hand(matching.market, endpoint, item);
                on_hand_total += held;
                staged_items.insert(item, held + matching.incoming((endpoint, item)));
            }
            let staged = staged_items.values().sum();
            let committed = matching.book.warehouse_committed(
                matching.market,
                endpoint.soul,
                endpoint.building,
                spec,
            );
            warehouses.push(Warehouse {
                id,
                endpoint,
                spec,
                items,
                policy: matching.book.warehouse_policy(id, spec),
                dispatch,
                staffed: assigned_driver(world, endpoint, id).is_some(),
                on_hand: on_hand_total,
                staged,
                staged_items,
                committed,
            });
        }
    }

    for urgent in [true, false] {
        for warehouse in &mut warehouses {
            if !released_sources.contains(&warehouse.endpoint) {
                matching.warehouse_output(
                    warehouse,
                    &inputs,
                    &staffed_producers,
                    &stations,
                    urgent,
                );
            }
            if !urgent {
                // A zero low watermark restocks once the warehouse is empty.
                if warehouse.staged < warehouse.spec.low_water_units.max(1) {
                    matching.book.restocking.insert(warehouse.id);
                } else if warehouse.staged >= warehouse.spec.high_water_units {
                    matching.book.restocking.remove(&warehouse.id);
                }
            }
        }
        for &(id, source, policy, recipe) in &sources {
            let has_driver = assigned_driver(world, source, id).is_some();
            if urgent && !has_driver {
                continue;
            }
            let has_fleet = has_fleet(world, id);
            let distance = |dest| straight_distance(map, source, dest);
            for output in &recipe.production {
                let item = output.id;
                if household_items().contains(&item) {
                    continue;
                }
                // Drop any sell order carried into this version from the old instantaneous lane.
                if !urgent {
                    matching.market.hide_freight_stock(source.soul, item);
                }
                let mut available = on_hand(matching.market, source, item);
                if available == 0 {
                    continue;
                }
                let mut candidates = inputs.get(&item).cloned().unwrap_or_default();
                candidates.sort_unstable_by_key(|input| {
                    (
                        on_hand(matching.market, input.destination, item) >= input.amount,
                        distance(input.destination),
                    )
                });
                // Open routes are served first. Suspended routes that still reach their
                // consumer keep their reorder on hand from what is left.
                let mut local = false;
                let mut held = Vec::new();
                for input in candidates {
                    if urgent && !matching.short(input, item) {
                        continue;
                    }
                    if !matching.route(source, input.destination) {
                        if !urgent && matching.held_route(source, input.destination) {
                            local = true;
                            held.push(input);
                        }
                        continue;
                    }
                    local = true;
                    if !has_driver || available == 0 {
                        break;
                    }
                    available -=
                        matching.reserve_for_input(policy, source, input, item, available, urgent);
                }
                if urgent {
                    continue;
                }
                if has_driver {
                    for input in held {
                        available -= matching.wanted(policy, input, item, available);
                    }
                }

                // Keep stock on hand for the next local reorder. Growable
                // factories have small output shelves and need room to stage
                // a dispatchable batch of surplus when their shelves are full.
                if local && has_fleet {
                    let mut keep = policy.capacity;
                    if world.companies[id].comp.proto.is_growable() {
                        let buffer =
                            output.amount.saturating_mul(recipe.storage_multiplier + 1) as u32;
                        keep = keep.min(buffer.saturating_sub(policy.minimum));
                    }
                    available = available.saturating_sub(keep);
                }
                if has_driver
                    && available > 0
                    && !warehouses.is_empty()
                    && !recipe.consumption.iter().any(|input| input.id == item)
                {
                    available = matching.warehouse_input(
                        source,
                        item,
                        inputs.get(&item).map(Vec::as_slice).unwrap_or_default(),
                        &mut warehouses,
                        &stations,
                        available,
                    );
                }
                if available == 0 || item.prototype().optout_exttrade {
                    continue;
                }

                let physical_station = (has_driver
                    && !world.companies[id].comp.proto.is_growable())
                .then(|| {
                    stations
                        .iter()
                        .copied()
                        .filter(|&station| matching.route(source, station))
                        .min_by_key(|&station| distance(station))
                })
                .flatten();
                if let Some(station) = physical_station {
                    let pending: u32 = matching
                        .book
                        .shipments
                        .values()
                        .filter(|s| {
                            s.source == source
                                && s.destination == station
                                && s.quantity.item == item
                        })
                        .map(|s| s.quantity.units)
                        .sum();
                    let units = available.min(policy.capacity.saturating_sub(pending));
                    if units != 0 {
                        matching.queue(source, station, item, units, false);
                    }
                } else if let Some(station) = stations
                    .iter()
                    .copied()
                    .min_by_key(|&station| distance(station))
                    .filter(|_| available >= policy.minimum)
                {
                    let trade =
                        matching
                            .market
                            .freight_export(source.soul, station.soul, item, available);
                    matching
                        .market
                        .produce(source.soul, item, -(available as i32));
                    post_freight_trade(world, resources, trade);
                }
            }
        }
    }
}

fn dispatch(
    book: &mut CargoBook,
    world: &mut World,
    resources: &Resources,
    map: &Map,
    now: u64,
    released_sources: &BTreeSet<Endpoint>,
) {
    let market = resources.read::<Market>();
    let time = resources.read::<prototypes::GameTime>();
    let infos = resources.read::<BuildingInfos>();
    // Warehouses may dispatch more than one truck. An earlier ineligible route does not
    // hold a later ready route. A destination that cannot run its recipe goes
    // first, then the oldest shipment, and shipment IDs break all ties.
    let mut routes = BTreeMap::<_, Vec<_>>::new();
    for (&id, shipment) in &book.shipments {
        routes
            .entry((shipment.source, shipment.destination))
            .or_default()
            .push(id);
    }
    let mut routes: Vec<_> = routes
        .into_iter()
        .map(|((source, destination), ids)| {
            let stocked_out = ids.iter().any(|id| {
                let item = book.shipments[id].quantity.item;
                destination_input(world, map, &infos, destination, item)
                    .is_some_and(|input| on_hand(&market, destination, item) < input.amount)
            });
            ((!stocked_out, ids[0]), source, destination, ids)
        })
        .collect();
    routes.sort_unstable_by_key(|&(order, ..)| order);
    let mut in_transit = book.in_transit();
    for (_, source, destination, ids) in routes {
        if released_sources.contains(&source) {
            continue;
        }
        let SoulID::GoodsCompany(company_id) = source.soul else {
            continue;
        };
        let mut drivers: Vec<_> = assigned_drivers(world, source, company_id)
            .filter(|&(driver, truck)| {
                let human = &world.humans[driver];
                let work = human.work.as_ref().unwrap();
                matches!(
                    work.kind,
                    WorkKind::Driver {
                        load: None,
                        deliver_order: None,
                        ..
                    }
                ) && work.work_inter.is_active(&time.daytime)
                    && !work.shift_ending(&time, 5)
                    && human.location == Location::Building(source.building)
                    && world.vehicles[truck].vehicle.state.is_parked()
            })
            .collect();
        drivers.sort_unstable();
        if drivers.is_empty() {
            continue;
        }
        let proto = world.companies[company_id].comp.proto.prototype();
        let Some(policy) = DispatchPolicy::for_prototype(proto) else {
            continue;
        };

        let mut remaining = ids;
        for (driver, truck) in drivers {
            if remaining.is_empty() {
                break;
            }
            let ids = &remaining;
            let mut room = BTreeMap::new();
            let mut urgent = false;
            let mut reserved_by_item = BTreeMap::<_, u32>::new();
            for id in ids {
                let ItemQuantity { item, units } = book.shipments[id].quantity;
                room.entry(item).or_insert_with(|| {
                    let Some(input) = destination_input(world, map, &infos, destination, item)
                    else {
                        // Warehouses reserve shared capacity at match time, and
                        // stations have no per-item unloading limit.
                        return u32::MAX;
                    };
                    let stock = on_hand(&market, destination, item);
                    urgent |= stock < input.amount;
                    let incoming = in_transit.get(&(destination, item)).copied().unwrap_or(0);
                    input
                        .storage_limit(policy)
                        .saturating_sub(stock.saturating_add(incoming))
                });
                *reserved_by_item.entry(item).or_default() += units;
            }
            let reserved = reserved_by_item.iter().fold(0_u32, |sum, (item, &units)| {
                sum.saturating_add(units.min(room[item]))
            });
            let free = room
                .values()
                .fold(0_u32, |sum, &units| sum.saturating_add(units));
            let age = now.saturating_sub(book.shipments[&ids[0]].reserved_at);
            let mut take = policy.quantity(reserved, free, age, urgent);
            if take == 0 {
                break;
            }

            let mut items = BTreeMap::<_, u32>::new();
            for id in ids {
                let shipment = book.shipments.get_mut(id).unwrap();
                let ItemQuantity { item, units } = shipment.quantity;
                let room = room.get_mut(&item).unwrap();
                let loaded = units.min(take).min(*room);
                if loaded == 0 {
                    continue;
                }
                *items.entry(item).or_default() += loaded;
                *in_transit.entry((destination, item)).or_default() += loaded;
                take -= loaded;
                *room -= loaded;
                shipment.quantity.units -= loaded;
                if shipment.quantity.units == 0 {
                    book.shipments.remove(id);
                }
                if take == 0 {
                    break;
                }
            }
            let from = map.buildings()[source.building].door_pos;
            let to = map.buildings()[destination.building].door_pos;
            let nominal_travel = driving_ticks(from.distance(to));
            // Allow for loading, parking and delays in addition to travel.
            let due_after = (DELIVERY_LEAD + nominal_travel).min(DELIVERY_EXPIRY);
            let load = LoadId(book.next_token());
            book.returning.remove(&truck);
            book.loads.insert(
                load,
                TruckLoad {
                    vehicle: truck,
                    source,
                    destination,
                    driver,
                    items: items
                        .into_iter()
                        .map(|(item, units)| ItemQuantity { item, units })
                        .collect(),
                    departed_at: now,
                    due_at: now + due_after,
                    used_truck: false,
                    routing_error_since: None,
                },
            );
            world.edit_human(driver, |human| {
                let work = human.work.as_mut().unwrap();
                work.kind = WorkKind::Driver {
                    truck,
                    deliver_order: Some(destination.building),
                    load: Some(load),
                };
            });
            // Do not replace an existing decision. The work scorer picks up this
            // assignment while the driver is at the depot.
            remaining.retain(|id| book.shipments.contains_key(id));
        }
    }
}

fn destination_input(
    world: &World,
    map: &Map,
    infos: &BuildingInfos,
    destination: Endpoint,
    item: ItemID,
) -> Option<Input> {
    let SoulID::GoodsCompany(id) = destination.soul else {
        return None;
    };
    let building = map.get(destination.building)?;
    let business =
        active_business(building, infos, world).filter(|business| business.company == id)?;
    let recipe = business.prototype.recipe.as_ref()?;
    let input = recipe.consumption.iter().find(|input| input.id == item)?;
    Some(Input::new(
        destination,
        input.amount,
        freight_batch_ticks(business)?,
    ))
}

/// Records the station side of a virtual export. The ordinary market still
/// owns its budget, stock and trade history. Imports never match at the market.
pub fn settle_physical_match(world: &mut World, _resources: &Resources, trade: Trade) -> bool {
    record_rail_handoff(world, trade);
    false
}

fn record_rail_handoff(world: &mut World, trade: Trade) {
    if let SoulID::FreightStation(id) = trade.buyer.0 {
        if let Some(station) = world.freight_stations.get_mut(id) {
            station.f.accept_export(trade.qty as u32);
        }
    }
}

fn post_freight_trade(world: &mut World, resources: &Resources, trade: Trade) {
    record_rail_handoff(world, trade);
    if trade.money_delta != Money::ZERO {
        Budget::apply(resources, BudgetCategory::TradeExport, trade.money_delta);
    }
    resources.write::<EcoStats>().record_trade(&trade);
}

/// Every binding a verified unload requires, checked in order.
#[allow(clippy::too_many_arguments)]
fn check_unload(
    world: &World,
    resources: &Resources,
    manifest: &TruckLoad,
    load: LoadId,
    driver: HumanID,
    truck: VehicleID,
    at: BuildingID,
    now: u64,
) -> Result<(), DeliveryRejection> {
    use DeliveryRejection::*;

    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let same_trip =
        manifest.driver == driver && manifest.vehicle == truck && manifest.destination() == at;
    let ends_live =
        manifest.source.live(world, &map, &infos) && manifest.destination.live(world, &map, &infos);
    let human = world.humans.get(driver);
    let driver_inside = human.is_some_and(|human| {
        human.location == Location::Building(at) && human.router.selected_vehicle() == Some(truck)
    });
    let holds_load = human
        .and_then(|human| human.work.as_ref())
        .is_some_and(|work| {
            work.workplace == manifest.source.building
                && matches!(work.kind, WorkKind::Driver {
                    truck: assigned,
                    load: Some(token),
                    deliver_order: Some(dest),
                } if assigned == truck && token == load && dest == at)
        })
        && matches!(manifest.source.soul, SoulID::GoodsCompany(id)
            if owns_assignment(world, manifest.source, id, driver, truck));
    let truck_parked = world
        .vehicles
        .get(truck)
        .is_some_and(|entity| entity.vehicle.state.is_parked());

    if !same_trip {
        Err(WrongTrip)
    } else if !manifest.used_truck {
        Err(TruckNotUsed)
    } else if now >= manifest.departed_at + DELIVERY_EXPIRY {
        Err(Expired)
    } else if !ends_live {
        Err(EndpointGone)
    } else if !driver_inside {
        Err(DriverNotAtDestination)
    } else if !holds_load {
        Err(AssignmentChanged)
    } else if !truck_parked {
        Err(TruckNotParked)
    } else {
        Ok(())
    }
}

/// Unload after the assigned driver rode the assigned truck to the live recipient.
pub fn complete_delivery(
    sim: &mut Simulation,
    load: LoadId,
    driver: HumanID,
    truck: VehicleID,
    at: BuildingID,
) -> Result<(), DeliveryRejection> {
    let (world, resources) = sim.world_res();
    let mut book = resources.write::<CargoBook>();
    let now = elapsed_ticks(resources).0;
    let manifest = book
        .loads
        .get(&load)
        .ok_or(DeliveryRejection::UnknownLoad)?;
    check_unload(world, resources, manifest, load, driver, truck, at, now)?;

    let manifest = book.loads.remove(&load).unwrap();
    clear_assignment(world, &manifest, load);
    book.returning.insert(
        truck,
        EmptyReturn {
            driver,
            depot: manifest.source,
            started_at: now,
            routing_error_since: None,
        },
    );
    let mut market = resources.write::<Market>();
    let units = manifest.units();
    let late = now > manifest.due_at;
    for &ItemQuantity { item, units } in &manifest.items {
        let trade = if matches!(manifest.destination.soul, SoulID::FreightStation(_)) {
            market.freight_export(manifest.source.soul, manifest.destination.soul, item, units)
        } else {
            market.receive_freight(manifest.destination.soul, item, units);
            book.day_mut(now)
                .receipt_mut(manifest.destination.soul, item)
                .road_units += u64::from(units);
            Trade {
                buyer: TradeTarget(manifest.destination.soul),
                seller: TradeTarget(manifest.source.soul),
                kind: item,
                qty: units as i32,
                money_delta: Money::ZERO,
            }
        };
        post_freight_trade(world, resources, trade);
    }
    book.day_mut(now).record_delivery(units, late);
    super::record_freight_outcome(resources, now, units, late);
    Ok(())
}

/// Settles a load at its destination. A refused unload cancels the load: its
/// stock returns to the source, the truck goes back to its depot and the
/// reason is logged and counted.
pub(crate) fn deliver_load(
    sim: &mut Simulation,
    load: LoadId,
    driver: HumanID,
    truck: VehicleID,
    at: BuildingID,
) {
    let reason = match complete_delivery(sim, load, driver, truck, at) {
        Ok(()) | Err(DeliveryRejection::UnknownLoad) => return,
        Err(reason) => reason,
    };
    log::warn!("freight load {load:?} refused at {at:?}: {reason:?}");
    let (world, resources) = sim.world_res();
    let mut book = resources.write::<CargoBook>();
    let now = elapsed_ticks(resources).0;
    book.day_mut(now).refused_unloads += 1;
    release(
        &mut book,
        world,
        resources,
        Vec::new(),
        vec![load],
        Release::Cancelled,
    );
}

#[derive(Clone, Copy)]
pub(crate) enum CargoRemoval {
    Building(BuildingID),
    Soul(SoulID),
    Driver(HumanID),
    Truck(VehicleID),
}

pub(crate) fn cancel_removed(sim: &mut Simulation, removal: CargoRemoval) {
    let (world, resources) = sim.world_res();
    let mut book = resources.write::<CargoBook>();
    if let CargoRemoval::Building(building) = removal {
        book.routes.forget(building);
    }
    let endpoint = |end: Endpoint| match removal {
        CargoRemoval::Building(id) => end.building == id,
        CargoRemoval::Soul(id) => end.soul == id,
        _ => false,
    };
    let shipments = book
        .shipments
        .iter()
        .filter_map(|(&id, s)| {
            let source_affected = match s.source.soul {
                SoulID::GoodsCompany(company) => {
                    world.companies.get(company).is_some_and(|entity| {
                        let proto = entity.comp.proto.prototype();
                        proto.storage.is_none()
                            && proto.n_trucks == 1
                            && (matches!(removal, CargoRemoval::Driver(driver)
                                if entity.comp.driver == Some(driver))
                                || matches!(removal, CargoRemoval::Truck(truck)
                                if entity.comp.trucks.contains(&truck)))
                    })
                }
                _ => false,
            };
            (source_affected || endpoint(s.source) || endpoint(s.destination)).then_some(id)
        })
        .collect();
    let loads = book
        .loads
        .iter()
        .filter_map(|(&id, load)| {
            (endpoint(load.source)
                || endpoint(load.destination)
                || matches!(removal, CargoRemoval::Driver(driver) if load.driver == driver)
                || matches!(removal, CargoRemoval::Truck(truck) if load.vehicle == truck))
            .then_some(id)
        })
        .collect();
    cancel_removed_imports(&mut book, resources, removal, endpoint);
    release(
        &mut book,
        world,
        resources,
        shipments,
        loads,
        Release::Cancelled,
    );
}

/// A removed station loses its import stock and fleet. A removed buyer's open
/// orders close; its loaded trucks return their cargo to the station.
fn cancel_removed_imports(
    book: &mut CargoBook,
    resources: &Resources,
    removal: CargoRemoval,
    endpoint: impl Fn(Endpoint) -> bool,
) {
    let mut market = resources.write::<Market>();
    if let CargoRemoval::Truck(truck) = removal {
        if let Some(StationTruck {
            station,
            trip: Some(trip),
        }) = book.station_trucks.remove(&truck)
        {
            match book.imports.get_mut(&station) {
                Some(desk) => desk.restock(&trip.items),
                None => trip
                    .items
                    .iter()
                    .for_each(|line| market.record_freight_loss(line.item, line.units)),
            }
        }
        return;
    }
    let removed: Vec<_> = book
        .imports
        .keys()
        .copied()
        .filter(|&station| endpoint(station))
        .collect();
    for station in removed {
        let desk = book.imports.remove(&station).unwrap();
        for (item, mut units) in desk.stock {
            while units > 0 {
                let loss = bounded_transfer(units, u32::MAX);
                market.record_freight_loss(item, loss);
                units -= u64::from(loss);
            }
        }
    }
    let commands = resources.read::<ParCommandBuffer<VehicleEnt>>();
    book.station_trucks.retain(|&truck, fleet| {
        if !endpoint(fleet.station) {
            return true;
        }
        for line in fleet.trip.iter().flat_map(|trip| &trip.items) {
            market.record_freight_loss(line.item, line.units);
        }
        commands.kill(truck);
        false
    });
    for desk in book.imports.values_mut() {
        desk.orders.retain(|order| !endpoint(order.buyer));
    }
}

fn clear_assignment(world: &mut World, manifest: &TruckLoad, token: LoadId) {
    world.edit_human(manifest.driver, |human| {
        let Some(WorkKind::Driver {
            load,
            deliver_order,
            ..
        }) = human.work.as_mut().map(|work| &mut work.kind)
        else {
            return;
        };
        if *load == Some(token) {
            *load = None;
            *deliver_order = None;
        }
    });
}

fn release(
    book: &mut CargoBook,
    world: &mut World,
    resources: &Resources,
    shipments: Vec<ShipmentId>,
    loads: Vec<LoadId>,
    reason: Release,
) {
    if shipments.is_empty() && loads.is_empty() {
        return;
    }
    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let now = elapsed_ticks(resources).0;
    let mut market = resources.write::<Market>();
    let mut restore = |source: Endpoint, quantity: ItemQuantity, world: &World| {
        if source.live(world, &map, &infos) {
            market.produce(source.soul, quantity.item, quantity.units as i32);
        } else {
            market.record_freight_loss(quantity.item, quantity.units);
        }
    };
    for id in shipments {
        let shipment = book.shipments.remove(&id).unwrap();
        restore(shipment.source, shipment.quantity, world);
    }
    for id in loads {
        let load = book.loads.remove(&id).unwrap();
        for &quantity in &load.items {
            restore(load.source, quantity, world);
        }
        clear_assignment(world, &load, id);
        let (driver, truck, used_truck) = (load.driver, load.vehicle, load.used_truck);
        resources
            .read::<ParCommandBuffer<HumanEnt>>()
            .exec_ent(driver, move |sim| {
                crate::souls::lifecycle::abort_freight_trip(sim, driver, truck, id, used_truck);
            });
        let day = book.day_mut(now);
        match reason {
            Release::Cancelled => day.record_cancellation(load.units()),
            Release::Withdrawn => day.withdrawn_loads += 1,
        }
    }
}

/// A route error, or the driver or the truck they ride waiting for a reroute.
/// The wait between reroute attempts counts down to zero for a tick before
/// each attempt, so the whole wait counts until a route is found.
fn routing_failed(human: &HumanEnt, vehicle: &crate::VehicleEnt, truck: VehicleID) -> bool {
    let rerouting = |it: &Itinerary| it.is_wait_for_reroute().is_some();
    human.router.last_error.is_some()
        || rerouting(&human.it)
        || (human.location == Location::Vehicle(truck) && rerouting(&vehicle.it))
}

/// Follows empty trucks home. A return leg that keeps failing, outlasts a
/// delivery window or loses its driver gives the truck back to its depot and
/// frees the driver.
fn track_returns(book: &mut CargoBook, world: &World, resources: &Resources, now: u64) {
    let mut stranded = Vec::new();
    book.returning.retain(|&truck, trip| {
        let Some(vehicle) = world.vehicles.get(truck) else {
            return false;
        };
        let Some(human) = world.humans.get(trip.driver) else {
            stranded.push((trip.driver, truck));
            return false;
        };
        let still_assigned = human.work.as_ref().is_some_and(|work| {
            work.workplace == trip.depot.building
                && matches!(work.kind, WorkKind::Driver { truck: assigned, load: None, .. }
                    if assigned == truck)
        });
        let home = human.location == Location::Building(trip.depot.building)
            && vehicle.vehicle.state.is_parked();
        if home {
            return false;
        }
        if !still_assigned {
            // Nobody drives the truck back, so it is recalled.
            stranded.push((trip.driver, truck));
            return false;
        }
        let homebound =
            human.router.target_dest == Some(Destination::Building(trip.depot.building));
        if homebound && routing_failed(human, vehicle, truck) {
            trip.routing_error_since.get_or_insert(now);
        } else {
            trip.routing_error_since = None;
        }
        let gave_up = trip
            .routing_error_since
            .is_some_and(|since| now >= since + ROUTING_RETRY_WINDOW);
        if gave_up || now >= trip.started_at + DELIVERY_EXPIRY {
            stranded.push((trip.driver, truck));
            return false;
        }
        true
    });
    let commands = resources.read::<ParCommandBuffer<HumanEnt>>();
    for (driver, truck) in stranded {
        log::warn!("empty freight truck {truck:?} could not return to its depot; recalling it");
        commands.exec_ent(driver, move |sim| {
            crate::souls::lifecycle::recover_empty_truck(sim, driver, truck);
        });
    }
}

pub fn cargo_update(world: &mut World, resources: &mut Resources) {
    profiling::scope!("economy::cargo_update");
    let now = elapsed_ticks(resources).0;
    let mut book = resources.write::<CargoBook>();
    let match_due = now % FREIGHT_MATCH_INTERVAL == 0;
    // Taken before any early return: the flag is not saved, so it must not outlive the tick.
    let topology_edited = std::mem::take(&mut book.topology_edited);
    if !match_due
        && book.loads.is_empty()
        && book.shipments.is_empty()
        && book.returning.is_empty()
        && book
            .station_trucks
            .values()
            .all(|truck| truck.trip.is_none())
    {
        return;
    }
    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let book = &mut *book;

    let valid = |routes: &mut RouteCache, source: Endpoint, dest: Endpoint| {
        source.live(world, &map, &infos)
            && dest.live(world, &map, &infos)
            && routes.reaches(&map, source.building, dest.building)
    };
    // Not on a route cache rebuild: the cache is not saved, so a loaded game would
    // check off-cycle where the uninterrupted one does not.
    let check_queued = match_due || topology_edited;
    let mut shipments: BTreeSet<_> = book
        .shipments
        .iter()
        .filter(|_| check_queued)
        .filter_map(|(&id, shipment)| {
            (now >= shipment.reserved_at + TICKS_PER_BUDGET_DAY
                || !valid(&mut book.routes, shipment.source, shipment.destination)
                || !matches!(shipment.source.soul, SoulID::GoodsCompany(company)
                if assigned_driver(world, shipment.source, company).is_some()))
            .then_some(id)
        })
        .collect();
    let mut failed_routes = BTreeSet::new();
    let loads: Vec<_> = book
        .loads
        .iter_mut()
        .filter_map(|(&id, load)| {
            let Some(human) = world.humans.get(load.driver) else {
                return Some(id);
            };
            let Some(vehicle) = world.vehicles.get(load.vehicle) else {
                return Some(id);
            };
            load.used_truck |= human.location == Location::Vehicle(load.vehicle);
            let outbound =
                human.router.target_dest == Some(Destination::Building(load.destination()));
            if outbound && routing_failed(human, vehicle, load.vehicle) {
                let since = *load.routing_error_since.get_or_insert(now);
                if now >= since + ROUTING_RETRY_WINDOW {
                    failed_routes.insert((load.source, load.destination));
                    return Some(id);
                }
            } else {
                load.routing_error_since = None;
            }
            let holds_load = human.work.as_ref().is_some_and(|work| {
                matches!(work.kind, WorkKind::Driver {
                    load: Some(token),
                    deliver_order: Some(dest),
                    ..
                } if token == id && dest == load.destination())
            });
            let assigned = matches!(load.source.soul, SoulID::GoodsCompany(company)
                if owns_assignment(world, load.source, company, load.driver, load.vehicle));
            let on_the_road = human.location == Location::Vehicle(load.vehicle)
                && matches!(
                    vehicle.vehicle.state,
                    VehicleState::Driving | VehicleState::Panicking(_)
                );
            let stalled = on_the_road && {
                let mut progress = book.progress.get(&id).copied();
                let stalled = stalled(&mut progress, vehicle.trans.pos, now);
                book.progress
                    .extend(progress.map(|progress| (id, progress)));
                stalled
            };
            if !on_the_road {
                book.progress.remove(&id);
            }
            if stalled {
                log::warn!("freight load {id:?} has not moved for an hour; releasing it");
            }
            (now >= load.departed_at + DELIVERY_EXPIRY
                || !valid(&mut book.routes, load.source, load.destination)
                || !assigned
                || !holds_load
                || stalled)
                .then_some(id)
        })
        .collect();
    // Stock queued on a route that gave up is released with it, and the route
    // waits out its cooldown so the next match can choose another lane.
    for &(source, destination) in &failed_routes {
        log::warn!("freight route {source:?} -> {destination:?} kept failing; suspending it");
        book.suspend_route(&map, source, destination, now);
        shipments.extend(book.shipments.iter().filter_map(|(&id, shipment)| {
            (shipment.source == source && shipment.destination == destination).then_some(id)
        }));
    }
    let released_sources = shipments
        .iter()
        .map(|id| book.shipments[id].source)
        .chain(loads.iter().map(|id| book.loads[id].source))
        .collect();
    release(
        book,
        world,
        resources,
        shipments.into_iter().collect(),
        loads,
        Release::Cancelled,
    );
    book.progress.retain(|id, _| book.loads.contains_key(id));
    track_returns(book, world, resources, now);
    track_station_trucks(book, world, resources, &map, &infos, now);
    if match_due {
        book.suspended.retain(|_, route| now < route.until);
        match_shipments(book, world, resources, &map, &infos, now, &released_sources);
        if !book.shipments.is_empty() {
            dispatch(book, world, resources, &map, now, &released_sources);
        }
        record_warehouse_activity(book, world, &map, &infos, now);
        order_imports(book, world, resources, &map, &infos, now);
        dispatch_station_trucks(book, world, resources, &map, now);
    }
}

/// Records a moving truck's position and whether it has stood still for
/// [`LOAD_STALL_TICKS`].
fn stalled(progress: &mut Option<LoadProgress>, pos: geom::Vec3, now: u64) -> bool {
    let last = progress.get_or_insert(LoadProgress {
        at: pos,
        since: now,
    });
    if last.at.distance(pos) >= STALL_DISTANCE {
        *last = LoadProgress {
            at: pos,
            since: now,
        };
    }
    now >= last.since + LOAD_STALL_TICKS
}

/// Stations that can import: live, with a train and a rail route to the outside.
fn import_stations(
    routes: &mut RouteCache,
    world: &World,
    map: &Map,
    infos: &BuildingInfos,
) -> Vec<Endpoint> {
    if world.trains.is_empty() {
        return Vec::new();
    }
    world
        .freight_stations
        .iter()
        .filter_map(|(id, station)| {
            let endpoint = Endpoint {
                soul: SoulID::FreightStation(id),
                building: station.f.building,
            };
            (endpoint.live(world, map, infos)
                && routes.rail_served(map, endpoint.building, train_stop(&station.trans)))
            .then_some(endpoint)
        })
        .collect()
}

/// Units of the input the recipe uses during an import lead time.
fn lead_demand(input: Input) -> u64 {
    (IMPORT_LEAD * u64::from(input.amount)).div_ceil(input.recipe_ticks.max(1))
}

/// Stock an import reorder fills up to: one batch plus two lead times of
/// recipe runs. One truckload limits the lead-time buffer.
fn import_target(input: Input) -> u32 {
    (u64::from(input.amount) + 2 * lead_demand(input))
        .min(u64::from(STATION_TRUCK_CAPACITY))
        .max(u64::from(input.amount)) as u32
}

/// A buyer that imports reorders at this stock, before it runs out. The
/// reorder fills at least a quarter of the target, so trucks run well loaded.
fn import_reorder_point(input: Input) -> u32 {
    let target = import_target(input);
    (u64::from(input.amount) + lead_demand(input))
        .min(u64::from(target) * 3 / 4)
        .max(u64::from(input.amount)) as u32
}

/// Imports what local freight leaves short, from the nearest station with
/// train service that reaches the buyer by road, up to the import target.
/// A recipe input is ordered when its buy order is open and no truck on its
/// way brings enough. A buyer that imported it within a day and has no truck
/// coming reorders at the reorder point instead, so it need not run out first.
/// Orders that local deliveries made unnecessary close before loading.
fn order_imports(
    book: &mut CargoBook,
    world: &mut World,
    resources: &Resources,
    map: &Map,
    infos: &BuildingInfos,
    now: u64,
) {
    let stations = import_stations(&mut book.routes, world, map, infos);
    let market = resources.read::<Market>();
    for (station, desk) in &mut book.imports {
        if !station.live(world, map, infos) {
            continue;
        }
        desk.orders
            .retain(|order| order.buyer.live(world, map, infos));
    }
    // Only a truck on its way, and not overdue, defers an import. Stock queued
    // at a producer can wait hours for a driver or a full load.
    let local = book.arriving(Some(now));
    let local = |key| local.get(&key).copied().unwrap_or(0);

    // Close open orders that local stock already covers.
    for desk in book.imports.values_mut() {
        desk.orders.retain(|order| {
            let (buyer, item) = (order.buyer, order.quantity.item);
            destination_input(world, map, infos, buyer, item).is_none_or(|input| {
                u64::from(on_hand(&market, buyer, item)) + u64::from(local((buyer, item)))
                    < u64::from(import_target(input))
            })
        });
    }
    let mut pending = book.imports_pending();

    book.no_import_route.clear();
    book.import_customers.retain(|&(buyer, _), &mut last| {
        now < last + TICKS_PER_BUDGET_DAY && buyer.live(world, map, infos)
    });
    // Open buy orders and recent import customers, with the order's quantity.
    let mut wants = BTreeMap::<(Endpoint, ItemID), Option<u32>>::new();
    for (&item, single) in market.iter() {
        if single.optout_exttrade() {
            continue;
        }
        for (buyer, order) in single.buy_orders() {
            let SoulID::GoodsCompany(id) = buyer else {
                continue;
            };
            let Some(company) = world.companies.get(id) else {
                continue;
            };
            let buyer = Endpoint {
                soul: buyer,
                building: company.comp.building,
            };
            if company.comp.proto.prototype().storage.is_none() && buyer.live(world, map, infos) {
                wants.insert((buyer, item), Some(order.qty));
            }
        }
    }
    for &key in book.import_customers.keys() {
        wants.entry(key).or_insert(None);
    }
    for ((buyer, item), ordered) in wants {
        let Some(input) = destination_input(world, map, infos, buyer, item) else {
            continue;
        };
        let key = (buyer, item);
        let stock = u64::from(on_hand(&market, buyer, item));
        let imported = pending.get(&key).copied().unwrap_or(0);
        let short = ordered.is_some_and(|qty| local(key) < qty);
        let running_low = book.import_customers.contains_key(&key)
            && local(key) == 0
            && stock + imported < u64::from(import_reorder_point(input));
        if !short && !running_low {
            continue;
        }
        let target = import_target(input);
        let covered = stock + u64::from(local(key)) + imported;
        let units = bounded_transfer(u64::from(target).saturating_sub(covered), target);
        if units == 0 {
            continue;
        }
        let station = stations
            .iter()
            .copied()
            .filter(|station| book.routes.reaches(map, station.building, buyer.building))
            .min_by_key(|&station| straight_distance(map, buyer, station));
        let Some(station) = station else {
            book.no_import_route.insert(buyer);
            continue;
        };
        book.imports.entry(station).or_default().place(ImportOrder {
            buyer,
            quantity: ItemQuantity { item, units },
            ordered_at: now,
        });
        *pending.entry(key).or_default() += u64::from(units);
    }
    drop(market);
    let spawn = resources.read::<ParCommandBuffer<FreightStationEnt>>();
    for (station, desk) in &book.imports {
        let SoulID::FreightStation(id) = station.soul else {
            continue;
        };
        if let Some(entity) = world.freight_stations.get_mut(id) {
            entity.f.wanted_cargo = desk.wanted_cargo();
        }
        let fleet = book
            .station_trucks
            .values()
            .filter(|truck| truck.station == *station)
            .count();
        if desk.orders.is_empty() || fleet >= STATION_TRUCKS || !station.live(world, map, infos) {
            continue;
        }
        let station = *station;
        spawn.exec_ent(id, move |sim| {
            let Some(door) = sim.map().get(station.building).map(|b| b.door_pos) else {
                return;
            };
            for _ in fleet..STATION_TRUCKS {
                let truck = spawn_truck_at_depot(sim, door);
                sim.write::<CargoBook>().station_trucks.insert(
                    truck,
                    StationTruck {
                        station,
                        trip: None,
                    },
                );
            }
        });
    }
}

/// Sends idle station trucks to reachable, stocked-out recipes first. Within
/// each priority class, the oldest available order gets the next truck.
fn dispatch_station_trucks(
    book: &mut CargoBook,
    world: &World,
    resources: &Resources,
    map: &Map,
    now: u64,
) {
    let tick = resources.read::<prototypes::GameTime>().tick;
    let commands = resources.read::<ParCommandBuffer<VehicleEnt>>();
    let market = resources.read::<Market>();
    let urgent = |order: &ImportOrder| {
        let item = order.quantity.item;
        destination_input(world, order.buyer, item)
            .is_some_and(|input| on_hand(&market, order.buyer, item) < input.amount)
    };
    let CargoBook {
        imports,
        station_trucks,
        routes,
        ..
    } = book;
    for (&truck, fleet) in station_trucks.iter_mut() {
        let Some(vehicle) = world.vehicles.get(truck) else {
            continue;
        };
        if fleet.trip.is_some() || !matches!(vehicle.vehicle.state, VehicleState::ParkedAtDepot) {
            continue;
        }
        let Some(desk) = imports.get_mut(&fleet.station) else {
            continue;
        };
        let ImportDesk { stock, orders, .. } = desk;
        let mut attempted = BTreeSet::new();
        let Some((buyer, route)) = [true, false].into_iter().find_map(|priority| {
            orders
                .iter()
                .filter(|order| stock.get(&order.quantity.item).copied().unwrap_or(0) > 0)
                .filter(|order| urgent(order) == priority)
                .find_map(|order| {
                    let buyer = order.buyer;
                    if !attempted.insert(buyer)
                        || !routes.reaches(map, fleet.station.building, buyer.building)
                    {
                        return None;
                    }
                    let access = routes.access(map, buyer.building)?;
                    let route = Itinerary::route(
                        tick,
                        vehicle.trans.pos,
                        access.drive_pos,
                        map,
                        PathKind::Vehicle,
                    )?;
                    Some((buyer, route))
                })
        }) else {
            continue;
        };
        let mut room = STATION_TRUCK_CAPACITY;
        let mut items = BTreeMap::<_, u32>::new();
        let mut ordered_at = now;
        for priority in [true, false] {
            for order in orders
                .iter_mut()
                .filter(|order| order.buyer == buyer && urgent(order) == priority)
            {
                let ItemQuantity { item, units } = order.quantity;
                let Some(held) = stock.get_mut(&item) else {
                    continue;
                };
                let loaded = bounded_transfer(*held, units.min(room));
                if loaded == 0 {
                    continue;
                }
                order.quantity.units -= loaded;
                *held -= u64::from(loaded);
                room -= loaded;
                *items.entry(item).or_default() += loaded;
                ordered_at = ordered_at.min(order.ordered_at);
                if room == 0 {
                    break;
                }
            }
            if room == 0 {
                break;
            }
        }
        orders.retain(|order| order.quantity.units > 0);
        stock.retain(|_, units| *units > 0);
        fleet.trip = Some(ImportTrip {
            buyer,
            items: items
                .into_iter()
                .map(|(item, units)| ItemQuantity { item, units })
                .collect(),
            ordered_at,
            departed_at: now,
            returning: false,
            routing_error_since: None,
            progress: None,
        });
        commands.exec_ent(truck, move |sim| {
            unpark(sim, truck);
            if let Some(vehicle) = sim.world.vehicles.get_mut(truck) {
                vehicle.it = route;
            }
        });
    }
}

enum StationLeg {
    Unload,
    Abort,
    Park,
}

/// Follows station trucks. A truck unloads at the buyer when its route ends
/// and drives back. A trip that keeps failing to route, or outlasts a delivery
/// window, returns its cargo to the station stock and the truck to the station.
fn track_station_trucks(
    book: &mut CargoBook,
    world: &mut World,
    resources: &Resources,
    map: &Map,
    infos: &BuildingInfos,
    now: u64,
) {
    let legs: Vec<_> = book
        .station_trucks
        .iter_mut()
        .filter_map(|(&truck, fleet)| {
            let trip = fleet.trip.as_mut()?;
            let vehicle = world.vehicles.get(truck)?;
            let moving = matches!(
                vehicle.vehicle.state,
                VehicleState::Driving | VehicleState::Panicking(_)
            );
            if moving && vehicle.it.is_wait_for_reroute().is_some() {
                trip.routing_error_since.get_or_insert(now);
            } else {
                trip.routing_error_since = None;
            }
            let stalled = moving && stalled(&mut trip.progress, vehicle.trans.pos, now);
            if !moving {
                trip.progress = None;
            }
            if stalled {
                log::warn!("station truck {truck:?} has not moved for an hour; calling it back");
            }
            let gave_up = trip
                .routing_error_since
                .is_some_and(|since| now >= since + ROUTING_RETRY_WINDOW)
                || now >= trip.departed_at + DELIVERY_EXPIRY
                || stalled;
            let arrived = moving && vehicle.it.has_ended(0.0);
            match (trip.returning, arrived, gave_up) {
                (false, true, _) => Some((truck, StationLeg::Unload)),
                (false, false, true) => Some((truck, StationLeg::Abort)),
                (true, true, _) | (true, false, true) => Some((truck, StationLeg::Park)),
                _ => None,
            }
        })
        .collect();
    if legs.is_empty() {
        return;
    }
    let tick = resources.read::<prototypes::GameTime>().tick;
    for (truck, leg) in legs {
        let fleet = book.station_trucks.get_mut(&truck).unwrap();
        let station = fleet.station;
        let trip = fleet.trip.as_mut().unwrap();
        let items = std::mem::take(&mut trip.items);
        let buyer = trip.buyer;
        let restock = match leg {
            StationLeg::Unload if buyer.live(world, map, infos) => {
                unload_import(book, resources, station, buyer, &items, now);
                false
            }
            StationLeg::Park => false,
            StationLeg::Unload | StationLeg::Abort => true,
        };
        if restock {
            match book.imports.get_mut(&station) {
                Some(desk) => desk.restock(&items),
                None => {
                    let mut market = resources.write::<Market>();
                    for line in &items {
                        market.record_freight_loss(line.item, line.units);
                    }
                }
            }
        }
        let home = match leg {
            StationLeg::Unload => book.routes.access(map, station.building).and_then(|home| {
                Itinerary::route(
                    tick,
                    world.vehicles[truck].trans.pos,
                    home.drive_pos,
                    map,
                    PathKind::Vehicle,
                )
            }),
            StationLeg::Abort | StationLeg::Park => None,
        };
        let fleet = book.station_trucks.get_mut(&truck).unwrap();
        if let Some(route) = home {
            let trip = fleet.trip.as_mut().unwrap();
            trip.returning = true;
            trip.routing_error_since = None;
            trip.progress = None;
            world.vehicles[truck].it = route;
        } else {
            fleet.trip = None;
            park_station_truck(world, resources, map, truck, station);
        }
    }
}

fn unload_import(
    book: &mut CargoBook,
    resources: &Resources,
    station: Endpoint,
    buyer: Endpoint,
    items: &[ItemQuantity],
    now: u64,
) {
    for &ItemQuantity { item, units } in items {
        let trade = {
            let mut market = resources.write::<Market>();
            market.receive_freight(buyer.soul, item, units);
            market.freight_import(station.soul, buyer.soul, item, units)
        };
        book.day_mut(now)
            .receipt_mut(buyer.soul, item)
            .station_units += u64::from(units);
        book.import_customers.insert((buyer, item), now);
        Budget::apply(resources, BudgetCategory::TradeImport, trade.money_delta);
        resources.write::<EcoStats>().record_trade(&trade);
    }
}

/// Puts a station truck back in the station's yard, off the road.
fn park_station_truck(
    world: &mut World,
    resources: &Resources,
    map: &Map,
    truck: VehicleID,
    station: Endpoint,
) {
    let Some(vehicle) = world.vehicles.get_mut(truck) else {
        return;
    };
    if let Some(collider) = vehicle.collider.take() {
        resources
            .write::<TransportGrid>()
            .remove_maintain(collider.0);
    }
    vehicle.vehicle.state = VehicleState::ParkedAtDepot;
    vehicle.it = Itinerary::NONE;
    vehicle.speed.0 = 0.0;
    if let Some(building) = map.get(station.building) {
        vehicle.trans = geom::Transform::new(building.door_pos);
    }
}

/// Records what each warehouse is doing after a match and dispatch, or the
/// first reason it is idle.
fn record_warehouse_activity(
    book: &mut CargoBook,
    world: &World,
    map: &Map,
    infos: &BuildingInfos,
    now: u64,
) {
    book.activity.clear();
    let live = |id: CompanyID, building: BuildingID| {
        let endpoint = Endpoint {
            soul: SoulID::GoodsCompany(id),
            building,
        };
        endpoint.live(world, map, infos).then_some(endpoint)
    };
    let producers: Vec<_> = world
        .companies
        .iter()
        .filter_map(|(id, company)| {
            let proto = company.comp.proto.prototype();
            if proto.storage.is_some() || (proto.id.is_growable() && !book.growable_freight) {
                return None;
            }
            DispatchPolicy::for_prototype(proto)?;
            let recipe = proto.recipe.as_ref()?;
            Some((live(id, company.comp.building)?, recipe))
        })
        .collect();
    for (id, company) in &world.companies {
        let Some(spec) = company.comp.proto.prototype().storage.as_ref() else {
            continue;
        };
        let Some(warehouse) = live(id, company.comp.building) else {
            continue;
        };
        let building = warehouse.building;
        let activity = if book
            .loads
            .values()
            .any(|load| load.source() == building || load.destination() == building)
            || book.returning.values().any(|trip| trip.depot == warehouse)
        {
            WarehouseActivity::Moving
        } else if book
            .shipments
            .values()
            .any(|s| s.source == warehouse || s.destination == warehouse)
        {
            WarehouseActivity::Queued
        } else if assigned_driver(world, warehouse, id).is_none() {
            WarehouseActivity::NoWorkers
        } else if book.restocking.contains(&id) {
            let items = stored_items(spec);
            let in_reach = producers.iter().any(|&(producer, recipe)| {
                recipe
                    .production
                    .iter()
                    .any(|output| items.contains(&output.id))
                    && book.physical_route(map, producer, warehouse, now)
            });
            if in_reach {
                WarehouseActivity::WaitingForSupply
            } else {
                WarehouseActivity::NoSupplierInReach
            }
        } else {
            WarehouseActivity::NothingBelowWatermark
        };
        book.activity.insert(id, activity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z3_fix_wide_import_desk_preserves_legacy_bincode() {
        use common::saveload::{Bincode, Encoder};

        let item = ItemID::new("goods");
        let legacy = (
            BTreeMap::from([(item, u32::MAX)]),
            BTreeMap::from([(item, 1_u32)]),
            Vec::<ImportOrder>::new(),
        );
        let bytes = Bincode::encode(&legacy).unwrap();
        let mut desk: ImportDesk = Bincode::decode(&bytes).unwrap();
        assert_eq!(Bincode::encode(&desk).unwrap(), bytes);
        *desk.stock.get_mut(&item).unwrap() += 2;
        let bytes = Bincode::encode(&desk).unwrap();
        let restored: ImportDesk = Bincode::decode(&bytes).unwrap();
        assert_eq!(restored.stock[&item], u64::from(u32::MAX) + 2);
    }

    #[test]
    fn z3_accounting_large_batch_keeps_station_reorder_bounded() {
        let input = Input {
            destination: Endpoint {
                soul: SoulID::GoodsCompany(CompanyID::default()),
                building: BuildingID::default(),
            },
            amount: i32::MAX as u32,
            recipe_ticks: 1,
        };
        assert_eq!(import_target(input), input.amount);
        assert_eq!(import_reorder_point(input), input.amount);
    }

    #[test]
    fn z3_accounting_faster_batches_raise_road_and_station_targets() {
        let endpoint = Endpoint {
            soul: SoulID::GoodsCompany(CompanyID::default()),
            building: BuildingID::default(),
        };
        let policy = DispatchPolicy {
            capacity: 16,
            minimum: 8,
            max_wait: 2 * TICKS_PER_HOUR,
        };
        let normal = Input::new(endpoint, 2, 6.0 * TICKS_PER_HOUR as f64);
        let faster = Input::new(endpoint, 2, 3.0 * TICKS_PER_HOUR as f64);
        assert!(faster.target(policy) > normal.target(policy));
        assert!(import_target(faster) > import_target(normal));
        assert!(import_reorder_point(faster) > import_reorder_point(normal));
        assert!(import_target(faster) <= STATION_TRUCK_CAPACITY);
    }

    #[test]
    fn e2a_dispatch_full_partial_aged_and_urgent() {
        let policy = DispatchPolicy {
            capacity: 16,
            minimum: 8,
            max_wait: 2 * prototypes::TICKS_PER_HOUR,
        };
        assert_eq!(policy.quantity(25, 32, 0, false), 16);
        assert_eq!(policy.quantity(25, 5, 0, false), 5);
        assert_eq!(policy.quantity(7, 32, policy.max_wait - 1, false), 0);
        assert_eq!(policy.quantity(7, 32, policy.max_wait, false), 7);
        assert_eq!(policy.quantity(3, 32, 0, true), 3);
        assert_eq!(policy.quantity(16, 0, policy.max_wait, true), 0);
    }

    #[test]
    fn freight2_configured_fleets_cover_nominal_production() {
        let _fixture = crate::tests::TestCtx::new_fast();
        let round_trip = driving_ticks(2_000.0) + HANDLING_TICKS;
        let trips = 10 * TICKS_PER_HOUR / round_trip;
        assert_eq!(trips, 4);
        for proto in prototypes::prototypes_iter::<GoodsCompanyPrototype>() {
            let (Some(policy), Some(recipe)) =
                (DispatchPolicy::for_prototype(proto), proto.recipe.as_ref())
            else {
                continue;
            };
            let units: i32 = recipe
                .production
                .iter()
                .filter(|output| uses_road_freight(proto, output.id, true))
                .map(|output| output.amount)
                .sum();
            let daily_output = (TICKS_PER_BUDGET_DAY as f64 * f64::from(units)
                / recipe.batch_ticks())
            .ceil() as u64;
            let haulage = u64::from(proto.n_trucks) * u64::from(policy.capacity) * trips;
            assert!(
                haulage >= daily_output,
                "{}: {haulage} < {daily_output}",
                proto.name
            );
            assert!(proto.n_trucks <= proto.n_workers, "{}", proto.name);
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::economy::{DailyEconomyMetrics, FreightReceipt};
    use crate::map::BuildingKind;
    use crate::map_dynamic::RouterError;
    use crate::souls::desire::Work;
    use crate::souls::goods_company::company_soul;
    use crate::souls::human::spawn_human;
    use crate::tests::TestCtx;
    use crate::{VehicleEnt, WorldCommand, INITIAL_TICK};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{BuildingGen, GameTime, GoodsCompanyID, RecTimeInterval};

    fn fixture_input(sim: &Simulation, destination: Endpoint, item: ItemID) -> Option<Input> {
        destination_input(
            sim.world(),
            &sim.map(),
            &sim.read::<BuildingInfos>(),
            destination,
            item,
        )
    }

    struct Fixture {
        ctx: TestCtx,
        source: Endpoint,
        destination: Endpoint,
        driver: HumanID,
        truck: VehicleID,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_companies("coal-mine", "coal-power-plant")
        }

        fn with_companies(source: &str, destination: &str) -> Self {
            Self::with_freight(source, destination, true)
        }

        fn with_freight(source: &str, destination: &str, growable_freight: bool) -> Self {
            let mut ctx = TestCtx::new_fast();
            ctx.g.write::<CargoBook>().growable_freight = growable_freight;
            let stations: Vec<_> = ctx
                .g
                .map()
                .buildings()
                .iter()
                .filter_map(|(id, b)| {
                    matches!(b.kind, BuildingKind::RailFreightStation(_))
                        .then_some(WorldCommand::MapRemoveBuilding(id))
                })
                .collect();
            ctx.apply(&stations);
            ctx.build_roads(&[
                vec3(100.0, 100.0, 0.0),
                vec3(500.0, 100.0, 0.0),
                vec3(500.0, 500.0, 0.0),
                vec3(100.0, 500.0, 0.0),
                vec3(100.0, 100.0, 0.0),
            ]);
            let (north, south) = {
                let map = ctx.g.map();
                let road_at = |y| {
                    map.roads()
                        .iter()
                        .find(|(_, road)| {
                            road.points().first().y == y && road.points().last().y == y
                        })
                        .unwrap()
                        .0
                };
                (road_at(100.0), road_at(500.0))
            };
            let mut place = |name, position, road| {
                let proto = GoodsCompanyID::new(name);
                let building = ctx
                    .g
                    .map_mut()
                    .build_special_building(
                        &OBB::new(position, Vec2::X, 20.0, 20.0),
                        BuildingKind::GoodsCompany(proto),
                        BuildingGen::CenteredDoor {
                            vertical_factor: 1.0,
                        },
                        None,
                        Some(road),
                    )
                    .unwrap();
                ctx.g.write::<BuildingInfos>().insert(building);
                let soul = company_soul(&mut ctx.g, building, proto).unwrap();
                Endpoint { soul, building }
            };
            let source = place(source, vec2(250.0, 132.0), north);
            let destination = place(destination, vec2(300.0, 467.0), south);
            let house = ctx.build_house_near(vec2(100.0, 250.0));
            let driver = spawn_human(&mut ctx.g, house).unwrap();
            let SoulID::GoodsCompany(company) = source.soul else {
                unreachable!()
            };
            let world = ctx.g.world_mut_unchecked();
            let source_company = &mut world.companies[company];
            let truck = source_company.comp.trucks[0];
            source_company.workers.0.push(driver);
            source_company.comp.driver = Some(driver);
            let human = &mut world.humans[driver];
            human.location = Location::Building(source.building);
            let mut work = Work::new(
                source.building,
                WorkKind::Driver {
                    truck,
                    deliver_order: None,
                    load: None,
                },
                0.0,
            );
            work.work_inter = RecTimeInterval::always();
            human.work = Some(work);
            {
                let mut infos = ctx.g.write::<BuildingInfos>();
                infos.get_out(house, SoulID::Human(driver));
                infos.get_in(source.building, SoulID::Human(driver));
            }
            Self {
                ctx,
                source,
                destination,
                driver,
                truck,
            }
        }

        fn coal() -> ItemID {
            ItemID::new("coal")
        }

        fn hand(&self, endpoint: Endpoint) -> i32 {
            self.ctx
                .g
                .read::<Market>()
                .capital(endpoint.soul, Self::coal())
        }

        fn seed(&mut self, source: i32, destination: i32) {
            let mut market = self.ctx.g.write::<Market>();
            market.produce(self.source.soul, Self::coal(), source);
            market.receive_freight(self.destination.soul, Self::coal(), destination as u32);
        }

        fn at_tick(&mut self, tick: u64) {
            *self.ctx.g.write::<GameTime>() = GameTime::new(Tick(INITIAL_TICK + tick));
        }

        fn update(&mut self, tick: u64) {
            self.at_tick(tick);
            let (world, resources) = self.ctx.g.world_res();
            cargo_update(world, resources);
        }

        fn token(&self) -> LoadId {
            let book = self.ctx.g.read::<CargoBook>();
            *book.loads.keys().next().unwrap()
        }

        fn manifest_units(&self) -> u32 {
            self.ctx
                .g
                .read::<CargoBook>()
                .manifest_for(self.truck)
                .map_or(0, |load| load.items.iter().map(|line| line.units).sum())
        }

        fn reserved(&self) -> u32 {
            self.ctx
                .g
                .read::<CargoBook>()
                .stock_for(self.source.building)
                .get(&Self::coal())
                .map_or(0, |stock| stock.reserved)
        }
    }

    fn warehouse(f: &mut Fixture, name: &str) -> Endpoint {
        let x = if name == "small-depot" { 205.0 } else { 180.0 };
        let proto = GoodsCompanyID::new(name);
        let spec = prototypes::try_prototype(proto).expect("built-in warehouse prototype");
        place_warehouse(f, spec, vec2(x, 450.0))
    }

    fn place_warehouse(f: &mut Fixture, spec: &GoodsCompanyPrototype, center: Vec2) -> Endpoint {
        let road = f.ctx.g.map().buildings()[f.destination.building].connected_road;
        place_warehouse_on(f, spec, center, road)
    }

    fn place_warehouse_on(
        f: &mut Fixture,
        spec: &GoodsCompanyPrototype,
        center: Vec2,
        road: Option<crate::map::RoadID>,
    ) -> Endpoint {
        let proto = spec.id;
        let building = f
            .ctx
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(center, Vec2::X, spec.size.w, spec.size.h),
                BuildingKind::GoodsCompany(proto),
                spec.bgen,
                None,
                road,
            )
            .unwrap();
        f.ctx.g.write::<BuildingInfos>().insert(building);
        let soul = company_soul(&mut f.ctx.g, building, proto).unwrap();
        Endpoint { soul, building }
    }

    fn warehouse_driver(f: &mut Fixture, endpoint: Endpoint) -> (HumanID, VehicleID) {
        let house = f.ctx.build_house_near(vec2(100.0, 380.0));
        let driver = spawn_human(&mut f.ctx.g, house).unwrap();
        let SoulID::GoodsCompany(company) = endpoint.soul else {
            unreachable!()
        };
        let world = f.ctx.g.world_mut_unchecked();
        let company = &mut world.companies[company];
        let truck = company.comp.trucks[company.workers.0.len()];
        company.workers.0.push(driver);
        let human = &mut world.humans[driver];
        human.location = Location::Building(endpoint.building);
        let mut work = Work::new(
            endpoint.building,
            WorkKind::Driver {
                truck,
                deliver_order: None,
                load: None,
            },
            0.0,
        );
        work.work_inter = RecTimeInterval::always();
        human.work = Some(work);
        (driver, truck)
    }

    fn unload_fixture(f: &mut Fixture, driver: HumanID, truck: VehicleID, at: BuildingID) {
        let token = {
            let mut book = f.ctx.g.write::<CargoBook>();
            let (&token, load) = book
                .loads
                .iter_mut()
                .find(|(_, load)| load.vehicle == truck)
                .unwrap();
            load.used_truck = true;
            token
        };
        let human = &mut f.ctx.g.world_mut_unchecked().humans[driver];
        let Location::Building(origin) = human.location else {
            unreachable!()
        };
        human.location = Location::Building(at);
        human.router.use_vehicle(Some(truck));
        assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, at).is_ok());
        assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, at).is_err());
        f.ctx.g.world_mut_unchecked().humans[driver].location = Location::Building(origin);
    }

    #[test]
    fn e2b_both_warehouses_fill_to_high_and_drain_for_local_shortage() {
        for (name, capacity, fleet) in [("small-depot", 64, 2), ("large-warehouse", 256, 4)] {
            let mut f = Fixture::new();
            let depot = warehouse(&mut f, name);
            let proto = GoodsCompanyID::new(name).prototype();
            let spec = proto.storage.as_ref().unwrap();
            assert_eq!((spec.capacity_units, proto.n_trucks), (capacity, fleet));
            let (driver, truck) = warehouse_driver(&mut f, depot);
            let input = fixture_input(&f.ctx.g, f.destination, Fixture::coal()).unwrap();
            let policy =
                DispatchPolicy::for_prototype(GoodsCompanyID::new("coal-mine").prototype())
                    .unwrap();
            f.seed(1000, input.target(policy) as i32);
            for step in 1..=spec.high_water_units.div_ceil(16) + 1 {
                f.update(u64::from(step) * FREIGHT_MATCH_INTERVAL);
                let incoming = f
                    .ctx
                    .g
                    .read::<CargoBook>()
                    .stock_for(depot.building)
                    .get(&Fixture::coal())
                    .map_or(0, |stock| stock.incoming);
                let staged = ((DEPARTURES_PER_DRIVER + step - 1) * policy.capacity)
                    .min(spec.high_water_units);
                assert_eq!(f.hand(depot) as u32 + incoming, staged);
                if f.manifest_units() > 0 {
                    let (driver, truck) = (f.driver, f.truck);
                    unload_fixture(&mut f, driver, truck, depot.building);
                }
            }
            assert_eq!(f.hand(depot), spec.high_water_units as i32);
            let destination_stock = f.hand(f.destination);
            let source_stock = f.hand(f.source);
            {
                let mut market = f.ctx.g.write::<Market>();
                market.produce(f.destination.soul, Fixture::coal(), -destination_stock);
                market.produce(f.source.soul, Fixture::coal(), -source_stock);
            }
            f.update(100 * FREIGHT_MATCH_INTERVAL);
            let at = f.destination.building;
            unload_fixture(&mut f, driver, truck, at);
            assert_eq!(f.hand(f.destination), 16);
            assert!(f.hand(depot) < spec.high_water_units as i32);

            let mut direct = Fixture::new();
            direct.seed(16, 0);
            direct.update(FREIGHT_MATCH_INTERVAL);
            let (driver, truck, at) = (direct.driver, direct.truck, direct.destination.building);
            unload_fixture(&mut direct, driver, truck, at);
            assert_eq!(direct.hand(direct.destination), f.hand(f.destination));
        }
    }

    #[test]
    fn e2b_surplus_loads_respect_policy_and_demolition() {
        for name in ["small-depot", "large-warehouse"] {
            let mut f = Fixture::new();
            let depot = warehouse(&mut f, name);
            let spec = GoodsCompanyID::new(name)
                .prototype()
                .storage
                .as_ref()
                .unwrap();
            let (driver, truck) = warehouse_driver(&mut f, depot);
            let terminal = station(&mut f, true);
            let SoulID::FreightStation(station) = terminal.soul else {
                unreachable!()
            };
            f.ctx
                .apply(&[WorldCommand::MapRemoveBuilding(f.destination.building)]);
            f.ctx.g.write::<Market>().produce(
                depot.soul,
                Fixture::coal(),
                spec.high_water_units as i32 + 16,
            );
            f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
                building: depot.building,
                policy: WarehousePolicy::ExportSurplus,
            }]);
            f.update(FREIGHT_MATCH_INTERVAL);
            assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_some());
            f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
                building: depot.building,
                policy: WarehousePolicy::ReserveLocally,
            }]);
            assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_none());
            assert_eq!(f.hand(depot), spec.high_water_units as i32 + 16);
            assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 0);
            f.update(2 * FREIGHT_MATCH_INTERVAL);
            assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_none());
            f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
                building: depot.building,
                policy: WarehousePolicy::ExportSurplus,
            }]);
            f.update(3 * FREIGHT_MATCH_INTERVAL);
            unload_fixture(&mut f, driver, truck, terminal.building);
            assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 16);
            assert_eq!(f.hand(depot), spec.high_water_units as i32);
            f.ctx
                .g
                .write::<Market>()
                .produce(depot.soul, Fixture::coal(), 16);
            f.update(4 * FREIGHT_MATCH_INTERVAL);
            assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_some());
            f.ctx
                .apply(&[WorldCommand::MapRemoveBuilding(depot.building)]);
            f.ctx.tick_fast();
            assert!(f.ctx.g.get(truck).is_none());
            assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_none());
            assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 16);
            assert_eq!(
                f.ctx.g.read::<Market>().seller_losses()[&Fixture::coal()],
                u64::from(spec.high_water_units + 16)
            );
        }
    }

    #[test]
    fn e2b_surplus_respects_capacity_and_inbound_demolition_refunds() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        warehouse_driver(&mut f, depot);
        station(&mut f, true);
        let capacity = 64;
        let policy =
            DispatchPolicy::for_prototype(GoodsCompanyID::new("coal-mine").prototype()).unwrap();
        let input = fixture_input(&f.ctx.g, f.destination, Fixture::coal()).unwrap();
        f.seed(100, input.target(policy) as i32);
        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, Fixture::coal(), 48);
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: depot.building,
            policy: WarehousePolicy::ExportSurplus,
        }]);
        for step in 1..=2 {
            f.update(step * FREIGHT_MATCH_INTERVAL);
            let book = f.ctx.g.read::<CargoBook>();
            let incoming = book.stock_for(depot.building)[&Fixture::coal()].incoming;
            assert_eq!(f.hand(depot) as u32 + incoming, capacity);
            assert_eq!(
                book.manifest_for(f.truck).unwrap().destination(),
                depot.building
            );
            assert!(book.outgoing_loads(depot.building).next().is_none());
        }
        let before = f.hand(f.source);
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(depot.building)]);
        assert_eq!(f.hand(f.source), before + 16);
        assert!(f
            .ctx
            .g
            .read::<CargoBook>()
            .stock_for(depot.building)
            .is_empty());
        assert!(f.ctx.g.read::<CargoBook>().manifest_for(f.truck).is_none());
        f.ctx.tick_fast();
        assert!(f.ctx.g.get(f.truck).is_some());
        assert_eq!(f.hand(f.source), before + 16);
    }

    #[test]
    fn e2b_warehouse_destinations_do_not_trade_with_each_other() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        let warehouse = place_warehouse(
            &mut f,
            GoodsCompanyID::new("large-warehouse").prototype(),
            vec2(410.0, 450.0),
        );
        warehouse_driver(&mut f, depot);
        let (driver, truck) = warehouse_driver(&mut f, warehouse);
        f.ctx
            .g
            .write::<Market>()
            .produce(warehouse.soul, Fixture::coal(), 224);
        f.update(FREIGHT_MATCH_INTERVAL);
        let (source, destination) = {
            let book = f.ctx.g.read::<CargoBook>();
            assert!(book.stock_for(depot.building).is_empty());
            let load = book.manifest_for(truck).unwrap();
            (load.source(), load.destination())
        };
        assert_eq!(
            (source, destination),
            (warehouse.building, f.destination.building)
        );
        let before = f.hand(warehouse);
        f.ctx.apply(&[WorldCommand::MapRemoveBuilding(destination)]);
        assert_eq!(f.hand(warehouse), 224);
        assert!(before < 224);
        assert!(f
            .ctx
            .g
            .read::<CargoBook>()
            .stock_for(destination)
            .is_empty());
        assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_none());
        assert!(f.ctx.g.get(driver).is_some());
    }

    #[test]
    fn e2b_industrial_firm_dispatches_goods_and_its_truck_is_removed_on_decline() {
        let mut f = Fixture::with_freight("industrial-l1", "commercial-l1", true);
        let item = ItemID::new("goods");
        let mut market = f.ctx.g.write::<Market>();
        market.produce(f.source.soul, item, 16);
        drop(market);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.manifest_units(), 16);
        let (driver, truck, at) = (f.driver, f.truck, f.destination.building);
        unload_fixture(&mut f, driver, truck, at);
        assert_eq!(
            f.ctx.g.read::<Market>().capital(f.destination.soul, item),
            16
        );
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(f.source.building)]);
        f.ctx.tick_fast();
        assert!(f.ctx.g.get(truck).is_none());
    }

    #[test]
    fn e2b_guided_warehouses_unlock_independently_and_policy_validates() {
        use crate::economy::progression::{ProgressionMode, ProgressionState};
        use crate::world_command::{CommandOutcomes, Rejection};

        let mut f = Fixture::new();
        f.ctx.g.write::<ProgressionState>().mode = ProgressionMode::Guided;
        for (name, milestone) in [("small-depot", 2), ("large-warehouse", 3)] {
            let proto = prototypes::try_prototype(GoodsCompanyID::new(name))
                .expect("built-in warehouse prototype");
            let road = f.ctx.g.map().buildings()[f.destination.building].connected_road;
            let x = if milestone == 2 { 165.0 } else { 400.0 };
            let command = WorldCommand::MapBuildSpecialBuilding {
                pos: OBB::new(vec2(x, 447.0), Vec2::X, proto.size.w, proto.size.h),
                kind: BuildingKind::GoodsCompany(proto.id),
                gen: proto.bgen,
                zone: None,
                connected_road: road,
            };
            f.ctx.g.write::<ProgressionState>().highest_milestone = milestone - 1;
            f.ctx.apply(&[command.clone()]);
            assert_eq!(
                f.ctx.g.read::<CommandOutcomes>().last,
                [("MapBuildSpecialBuilding", Rejection::Locked { milestone })]
            );
            f.ctx.g.write::<ProgressionState>().highest_milestone = milestone;
            f.ctx.apply(&[command]);
            assert!(f.ctx.g.read::<CommandOutcomes>().last.is_empty());
        }
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: f.destination.building,
            policy: WarehousePolicy::ReserveLocally,
        }]);
        assert_eq!(
            f.ctx.g.read::<CommandOutcomes>().last,
            [("SetWarehousePolicy", Rejection::InvalidWarehouse)]
        );
    }

    #[test]
    fn e2a_full_load_conserves_stock_and_unloads_once_after_a_real_drive() {
        let mut fixture = Fixture::new();
        let capacity = mine_policy().capacity;
        fixture.seed(capacity as i32 + 9, 1);
        fixture.update(FREIGHT_MATCH_INTERVAL);
        let load = fixture.token();
        let (driver, truck, destination) =
            (fixture.driver, fixture.truck, fixture.destination.building);
        assert_eq!(fixture.manifest_units(), capacity);
        assert_eq!(fixture.reserved(), 9);
        assert_eq!(fixture.hand(fixture.source), 0);
        assert_eq!(fixture.hand(fixture.destination), 1);
        assert_eq!(
            fixture.ctx.g.read::<CargoBook>().stock_for(destination)[&Fixture::coal()].incoming,
            capacity + 9
        );
        assert!(complete_delivery(&mut fixture.ctx.g, load, driver, truck, destination).is_err());

        for _ in 0..5_000 {
            fixture.ctx.tick_fast();
            if fixture
                .ctx
                .g
                .read::<CargoBook>()
                .loads
                .get(&load)
                .is_some_and(|load| load.used_truck)
            {
                break;
            }
        }
        let mut resumed: Simulation =
            Bincode::decode(&Bincode::encode(&fixture.ctx.g).unwrap()).unwrap();
        assert!(resumed.read::<CargoBook>().loads[&load].used_truck);
        assert_eq!(fixture.ctx.g.hashes(), resumed.hashes());
        let mut schedule = Simulation::schedule();
        for _ in 0..10_000 {
            fixture.ctx.tick_fast();
            resumed.tick(&mut schedule, &[]);
            if !fixture.ctx.g.read::<CargoBook>().loads.contains_key(&load) {
                break;
            }
        }
        assert_eq!(fixture.hand(fixture.destination), capacity as i32 + 1);
        assert_eq!(fixture.manifest_units(), 0);
        assert_eq!(fixture.reserved(), 9);
        assert_eq!(fixture.ctx.g.hashes(), resumed.hashes());
        assert!(complete_delivery(&mut fixture.ctx.g, load, driver, truck, destination).is_err());
        let metrics = fixture.ctx.g.read::<DailyEconomyMetrics>();
        assert_eq!(metrics.days[&0].loads, Some(1));
        assert_eq!(metrics.days[&0].load_units, Some(u64::from(capacity)));
        assert_eq!(metrics.days[&0].late_loads, Some(0));
    }

    #[test]
    fn e2a_destination_reorder_and_aged_partial_dispatch_are_bounded() {
        let mut f = Fixture::new();
        let input = fixture_input(&f.ctx.g, f.destination, Fixture::coal()).unwrap();
        let policy =
            DispatchPolicy::for_prototype(GoodsCompanyID::new("coal-mine").prototype()).unwrap();
        f.seed(5, (input.target(policy) - policy.minimum) as i32);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(
            (f.manifest_units(), f.reserved(), f.hand(f.source)),
            (0, 5, 0)
        );
        // An independent delivery uses up the unloading headroom.
        f.seed(0, (policy.capacity + policy.minimum - 5) as i32);
        f.update(2 * FREIGHT_MATCH_INTERVAL);
        assert_eq!(
            (f.manifest_units(), f.reserved(), f.hand(f.source)),
            (5, 0, 0)
        );

        let mut f = Fixture::new();
        f.seed(7, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!((f.manifest_units(), f.reserved()), (0, 7));
        f.update(FREIGHT_MATCH_INTERVAL + policy.max_wait);
        assert_eq!((f.manifest_units(), f.reserved()), (7, 0));

        let mut f = Fixture::new();
        f.seed(3, 0);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(
            (f.manifest_units(), f.reserved(), f.hand(f.destination)),
            (3, 0, 0)
        );
    }

    #[test]
    fn e2a_cancel_on_demolition_before_and_after_dispatch() {
        for count in [5, 16] {
            let mut f = Fixture::new();
            f.seed(count, 1);
            f.update(FREIGHT_MATCH_INTERVAL);
            f.ctx
                .apply(&[WorldCommand::MapRemoveBuilding(f.destination.building)]);
            assert_eq!(
                (f.manifest_units(), f.reserved(), f.hand(f.source)),
                (0, 0, count)
            );
            f.ctx.tick_fast();
            assert_eq!(f.hand(f.source), count);
        }

        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(f.source.building)]);
        f.ctx.tick_fast();
        assert_eq!(f.hand(f.destination), 1);
        assert_eq!(
            f.ctx.g.read::<Market>().seller_losses()[&Fixture::coal()],
            16
        );
    }

    #[test]
    fn e2a_truck_destruction_driver_reassignment_road_removal_and_expiry_release() {
        for reason in ["truck", "driver", "road", "expiry"] {
            let mut f = Fixture::new();
            f.seed(16, 1);
            f.update(FREIGHT_MATCH_INTERVAL);
            assert_eq!(f.manifest_units(), 16);
            match reason {
                "truck" => {
                    f.ctx.g.read::<ParCommandBuffer<VehicleEnt>>().kill(f.truck);
                    f.ctx.tick_fast();
                }
                "driver" => {
                    f.ctx.g.world_mut_unchecked().humans[f.driver]
                        .work
                        .as_mut()
                        .unwrap()
                        .kind = WorkKind::Worker;
                    f.update(FREIGHT_MATCH_INTERVAL + 1);
                }
                "road" => {
                    let road = f.ctx.g.map().buildings()[f.destination.building]
                        .connected_road
                        .unwrap();
                    f.ctx.apply(&[WorldCommand::MapRemoveRoad(road)]);
                    f.update(FREIGHT_MATCH_INTERVAL + 1);
                }
                "expiry" => f.update(FREIGHT_MATCH_INTERVAL + DELIVERY_EXPIRY),
                _ => unreachable!(),
            }
            assert_eq!(
                (
                    f.manifest_units(),
                    f.reserved(),
                    f.hand(f.source),
                    f.hand(f.destination)
                ),
                (0, 0, 16, 1),
                "{reason}"
            );
            f.update(FREIGHT_MATCH_INTERVAL + DELIVERY_EXPIRY + 2);
            assert_eq!(f.hand(f.source), 16, "{reason}");
        }
    }

    fn station(f: &mut Fixture, connected: bool) -> Endpoint {
        let road = road_along(f, None, Some(500.0));
        station_at(f, vec2(370.0, 467.0), Some(road).filter(|_| connected))
    }

    fn station_at(f: &mut Fixture, position: Vec2, road: Option<crate::map::RoadID>) -> Endpoint {
        let proto = prototypes::FreightStationPrototypeID::new("freight-station");
        let building = f
            .ctx
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(position, Vec2::X, 12.0, 12.0),
                BuildingKind::RailFreightStation(proto),
                BuildingGen::CenteredDoor {
                    vertical_factor: 1.0,
                },
                None,
                road,
            )
            .unwrap();
        f.ctx.g.write::<BuildingInfos>().insert(building);
        let id = crate::souls::freight_station::freight_station_soul(&mut f.ctx.g, building, proto)
            .unwrap();
        Endpoint {
            soul: SoulID::FreightStation(id),
            building,
        }
    }

    #[test]
    fn e2a_startup_exports_when_mine_has_no_truck_or_road_route() {
        for no_truck in [false, true] {
            let mut f = Fixture::new();
            let terminal = station(&mut f, false);
            let SoulID::FreightStation(station) = terminal.soul else {
                unreachable!()
            };
            let road = f.ctx.g.map().buildings()[f.source.building]
                .connected_road
                .unwrap();
            if no_truck {
                f.ctx.g.read::<ParCommandBuffer<VehicleEnt>>().kill(f.truck);
                f.ctx.tick_fast();
            } else {
                f.ctx.apply(&[WorldCommand::MapRemoveRoad(road)]);
            }
            f.seed(16, 0);
            f.at_tick(FREIGHT_MATCH_INTERVAL - 1);
            f.ctx.tick_fast();
            assert_eq!((f.manifest_units(), f.reserved()), (0, 0));
            // The station has no road to the plant, so nothing is imported.
            assert_eq!(f.hand(f.destination), 0);
            let station = &f.ctx.g.get(station).unwrap().f;
            assert_eq!(station.wanted_cargo, 0);
            assert_eq!(station.waiting_cargo, 16);
            assert_eq!(f.hand(f.source), 0);
        }
    }

    #[test]
    fn e2a_station_waits_for_verified_unload_and_cancels_on_removal() {
        for remove in [false, true] {
            let mut f = Fixture::new();
            let terminal = station(&mut f, true);
            f.ctx
                .apply(&[WorldCommand::MapRemoveBuilding(f.destination.building)]);
            let SoulID::FreightStation(station) = terminal.soul else {
                unreachable!()
            };
            f.seed(16, 0);
            let export = |f: &Fixture| f.ctx.g.read::<Budget>().total(BudgetCategory::TradeExport);
            let before = export(&f);
            f.update(FREIGHT_MATCH_INTERVAL);
            let token = f.token();
            assert_eq!(f.manifest_units(), 16);
            assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 0);
            assert_eq!(export(&f), before);
            if remove {
                f.ctx
                    .apply(&[WorldCommand::MapRemoveBuilding(terminal.building)]);
                assert_eq!(f.manifest_units(), 0);
                assert_eq!(f.hand(f.source), 16);
                assert_eq!(export(&f), before);
            } else {
                let (driver, truck, at) = (f.driver, f.truck, terminal.building);
                f.ctx
                    .g
                    .write::<CargoBook>()
                    .loads
                    .get_mut(&token)
                    .unwrap()
                    .used_truck = true;
                let human = &mut f.ctx.g.world_mut_unchecked().humans[driver];
                human.location = Location::Building(at);
                human.router.use_vehicle(Some(truck));
                assert!(
                    complete_delivery(&mut f.ctx.g, token, driver, VehicleID::default(), at)
                        .is_err()
                );
                f.ctx
                    .g
                    .write::<BuildingInfos>()
                    .set_owner(at, f.destination.soul);
                assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, at).is_err());
                f.ctx
                    .g
                    .write::<BuildingInfos>()
                    .set_owner(at, terminal.soul);
                assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, at).is_ok());
                assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 16);
                let expected = f
                    .ctx
                    .g
                    .read::<Market>()
                    .freight_export(f.source.soul, terminal.soul, Fixture::coal(), 16)
                    .money_delta;
                assert_eq!(export(&f) - before, i128::from(expected.0));
                assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, at).is_err());
                assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 16);
            }
        }
    }

    #[test]
    fn e2a_serves_one_destination_per_trip_and_chooses_the_oldest_ready_route() {
        let mut f = Fixture::with_companies("flour-factory", "bakery");
        let terminal = station(&mut f, true);
        let flour = ItemID::new("flour");
        {
            let mut market = f.ctx.g.write::<Market>();
            market.produce(f.source.soul, flour, 32);
            market.receive_freight(f.destination.soul, flour, 1);
            let mut book = f.ctx.g.write::<CargoBook>();
            for (destination, reserved_at) in [(f.destination, 0), (terminal, 1)] {
                book.reserve(
                    &mut market,
                    Shipment {
                        source: f.source,
                        destination,
                        quantity: ItemQuantity {
                            item: flour,
                            units: 16,
                        },
                        reserved_at,
                    },
                );
            }
        }
        f.update(FREIGHT_MATCH_INTERVAL);
        let book = f.ctx.g.read::<CargoBook>();
        let load = book.manifest_for(f.truck).unwrap();
        assert_eq!(
            load.items,
            vec![ItemQuantity {
                item: flour,
                units: 16
            }]
        );
        assert_eq!(load.destination(), f.destination.building);
        assert_eq!(book.stock_for(terminal.building)[&flour].incoming, 16);
    }

    fn road_along(f: &Fixture, x: Option<f32>, y: Option<f32>) -> crate::map::RoadID {
        let map = f.ctx.g.map();
        map.roads()
            .iter()
            .find(|(_, road)| {
                let (a, b) = (road.points().first(), road.points().last());
                x.is_none_or(|x| a.x == x && b.x == x) && y.is_none_or(|y| a.y == y && b.y == y)
            })
            .unwrap()
            .0
    }

    fn incoming(f: &Fixture, building: BuildingID, item: ItemID) -> u32 {
        f.ctx
            .g
            .read::<CargoBook>()
            .stock_for(building)
            .get(&item)
            .map_or(0, |stock| stock.incoming)
    }

    #[test]
    fn freight2_absent_mill_driver_has_a_first_load_for_each_empty_bakery() {
        let mut f = Fixture::with_companies("flour-factory", "bakery");
        let road = road_along(&f, None, Some(100.0));
        let near = place_warehouse_on(
            &mut f,
            GoodsCompanyID::new("bakery").prototype(),
            vec2(310.0, 132.0),
            Some(road),
        );
        let far = f.destination;
        let flour = ItemID::new("flour");
        let policy =
            DispatchPolicy::for_prototype(GoodsCompanyID::new("flour-factory").prototype())
                .unwrap();
        f.ctx.g.world_mut_unchecked().edit_human(f.driver, |human| {
            human.location = Location::Outside;
        });
        f.ctx.g.write::<Market>().produce(f.source.soul, flour, 60);
        f.update(FREIGHT_MATCH_INTERVAL);
        let near_units = incoming(&f, near.building, flour);
        let far_units = incoming(&f, far.building, flour);
        assert_eq!(f.manifest_units(), 0);
        assert!(
            (1..=policy.capacity).contains(&near_units)
                && (1..=policy.capacity).contains(&far_units),
            "expected a first load for each bakery, got near={near_units}, far={far_units}"
        );

        let home = f.source.building;
        f.ctx.g.world_mut_unchecked().edit_human(f.driver, |human| {
            human.location = Location::Building(home);
        });
        f.update(2 * FREIGHT_MATCH_INTERVAL);
        let first = f
            .ctx
            .g
            .read::<CargoBook>()
            .manifest_for(f.truck)
            .unwrap()
            .destination();
        assert_eq!(first, near.building);
        let (driver, truck) = (f.driver, f.truck);
        unload_fixture(&mut f, driver, truck, first);
        f.update(3 * FREIGHT_MATCH_INTERVAL);
        let second = f
            .ctx
            .g
            .read::<CargoBook>()
            .manifest_for(truck)
            .unwrap()
            .destination();
        assert_eq!(second, far.building);
    }

    #[test]
    fn freight2_replenishment_respects_the_shift_and_departure_limit() {
        let mut f = Fixture::new();
        f.ctx.g.world_mut_unchecked().edit_human(f.driver, |human| {
            human.work.as_mut().unwrap().work_inter = RecTimeInterval::new((8, 0), (8, 10));
        });
        f.seed(1_000, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!((f.manifest_units(), f.reserved()), (0, 0));
        assert_eq!(f.hand(f.source), 1_000);

        f.ctx.g.world_mut_unchecked().edit_human(f.driver, |human| {
            human.work.as_mut().unwrap().work_inter = RecTimeInterval::new((8, 0), (18, 0));
            human.location = Location::Outside;
        });
        f.update(2 * FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.manifest_units(), 0);
        assert_eq!(f.reserved(), DEPARTURES_PER_DRIVER * mine_policy().capacity);
        f.update(3 * FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.reserved(), DEPARTURES_PER_DRIVER * mine_policy().capacity);
    }

    #[test]
    fn freight2_an_existing_mill_adds_a_truck_and_assigns_a_second_worker() {
        use crate::souls::goods_company::company_system;
        use crate::world::CompanyEnt;

        let mut f = Fixture::with_companies("flour-factory", "bakery");
        let SoulID::GoodsCompany(mill) = f.source.soul else {
            unreachable!()
        };
        let workplace = f.source.building;
        let old_fleet = &mut f.ctx.g.world_mut_unchecked().companies[mill].comp.trucks;
        assert_eq!(old_fleet.len(), 2);
        let extra = old_fleet.pop().unwrap();
        f.ctx.g.read::<ParCommandBuffer<VehicleEnt>>().kill(extra);
        ParCommandBuffer::<VehicleEnt>::apply(&mut f.ctx.g);
        f.ctx.g = Bincode::decode(&Bincode::encode(&f.ctx.g).unwrap()).unwrap();

        let house = f.ctx.build_house_near(vec2(100.0, 380.0));
        let second = spawn_human(&mut f.ctx.g, house).unwrap();
        let world = f.ctx.g.world_mut_unchecked();
        world.companies[mill].workers.0.push(second);
        world.edit_human(second, |human| {
            let mut work = Work::new(workplace, WorkKind::Worker, 0.0);
            work.work_inter = RecTimeInterval::always();
            human.work = Some(work);
            human.location = Location::Building(workplace);
        });
        for _ in 0..2 {
            let (world, resources) = f.ctx.g.world_res();
            company_system(world, resources);
            ParCommandBuffer::<CompanyEnt>::apply(&mut f.ctx.g);
        }
        let trucks = &f.ctx.g.world().companies[mill].comp.trucks;
        assert_eq!(trucks.len(), 2);
        assert_eq!(trucks[0], f.truck);
        assert!(matches!(
            f.ctx.g.world().humans[second].work.as_ref().unwrap().kind,
            WorkKind::Driver { truck, .. } if truck == trucks[1]
        ));

        f.ctx
            .g
            .write::<Market>()
            .produce(f.source.soul, ItemID::new("flour"), 108);
        f.update(FREIGHT_MATCH_INTERVAL);
        let book = f.ctx.g.read::<CargoBook>();
        assert_eq!(
            book.outgoing_loads(workplace)
                .map(TruckLoad::units)
                .collect::<Vec<_>>(),
            [54, 54]
        );
    }

    #[test]
    fn freight2_station_serves_an_empty_plant_before_an_older_restock() {
        let mut f = Fixture::new();
        let terminal = station(&mut f, true);
        let road = road_along(&f, None, Some(500.0));
        let store = place_warehouse_on(
            &mut f,
            GoodsCompanyID::new("commercial-l1").prototype(),
            vec2(425.0, 467.0),
            Some(road),
        );
        let plant = f.destination;
        let goods = ItemID::new("goods");
        f.ctx
            .g
            .write::<Market>()
            .receive_freight(store.soul, goods, 2);
        let door = f.ctx.g.map().buildings()[terminal.building].door_pos;
        let truck = spawn_truck_at_depot(&mut f.ctx.g, door);
        {
            let mut book = f.ctx.g.write::<CargoBook>();
            let desk = book.imports.entry(terminal).or_default();
            desk.stock.extend([(goods, 48), (Fixture::coal(), 39)]);
            for (buyer, item, units, ordered_at) in [
                (store, goods, 48, 1),
                (plant, Fixture::coal(), 13, 2),
                (plant, Fixture::coal(), 13, 3),
                (plant, Fixture::coal(), 13, 4),
            ] {
                desk.orders.push(ImportOrder {
                    buyer,
                    quantity: ItemQuantity { item, units },
                    ordered_at,
                });
            }
            book.station_trucks.insert(
                truck,
                StationTruck {
                    station: terminal,
                    trip: None,
                },
            );
        }
        let dispatch = |f: &Fixture, now| {
            dispatch_station_trucks(
                &mut f.ctx.g.write::<CargoBook>(),
                f.ctx.g.world(),
                &f.ctx.g.resources,
                &f.ctx.g.map(),
                now,
            );
        };
        dispatch(&f, FREIGHT_MATCH_INTERVAL);
        {
            let book = f.ctx.g.read::<CargoBook>();
            let trip = book.station_trucks[&truck].trip.as_ref().unwrap();
            assert_eq!(
                trip.buyer, plant,
                "the empty plant takes the first departure"
            );
            assert_eq!(
                trip.items,
                [ItemQuantity {
                    item: Fixture::coal(),
                    units: 39
                }]
            );
            assert_eq!(book.imports[&terminal].orders[0].buyer, store);
        }
        let next_truck = spawn_truck_at_depot(&mut f.ctx.g, door);
        f.ctx.g.write::<CargoBook>().station_trucks.insert(
            next_truck,
            StationTruck {
                station: terminal,
                trip: None,
            },
        );
        dispatch(&f, 2 * FREIGHT_MATCH_INTERVAL);
        let book = f.ctx.g.read::<CargoBook>();
        let trip = book.station_trucks[&next_truck].trip.as_ref().unwrap();
        assert_eq!(trip.buyer, store);
        assert_eq!(
            trip.items,
            [ItemQuantity {
                item: goods,
                units: 48
            }]
        );
        assert!(book.imports[&terminal].orders.is_empty());
    }

    fn mine_policy() -> DispatchPolicy {
        DispatchPolicy::for_prototype(GoodsCompanyID::new("coal-mine").prototype()).unwrap()
    }

    fn plant_target(f: &Fixture) -> i32 {
        fixture_input(&f.ctx.g, f.destination, Fixture::coal())
            .unwrap()
            .target(mine_policy()) as i32
    }

    fn set_router(f: &mut Fixture, vehicle: VehicleID, to: BuildingID, error: Option<RouterError>) {
        let human = &mut f.ctx.g.world_mut_unchecked().humans[f.driver];
        human.router.use_vehicle(Some(vehicle));
        human.router.go_to(Destination::Building(to));
        human.router.last_error = error;
    }

    fn drain_commands(f: &mut Fixture) {
        ParCommandBuffer::<HumanEnt>::apply(&mut f.ctx.g);
        ParCommandBuffer::<VehicleEnt>::apply(&mut f.ctx.g);
    }

    #[test]
    fn e2fix_failed_empty_return_releases_the_driver_and_keeps_the_truck_usable() {
        use crate::souls::human::HumanDecisionKind;

        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let (driver, truck) = (f.driver, f.truck);
        let (source, destination) = (f.source.building, f.destination.building);
        let token = f.token();
        f.ctx
            .g
            .write::<CargoBook>()
            .loads
            .get_mut(&token)
            .unwrap()
            .used_truck = true;
        {
            let human = &mut f.ctx.g.world_mut_unchecked().humans[driver];
            human.location = Location::Building(destination);
            human.router.use_vehicle(Some(truck));
        }
        assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, destination).is_ok());

        // The empty leg back to the depot cannot be planned.
        let personal = f.ctx.g.world().humans[driver].router.personal_car;
        f.ctx.g.world_mut_unchecked().humans[driver].decision.kind =
            HumanDecisionKind::MultiStack(vec![
                HumanDecisionKind::SetVehicle(personal),
                HumanDecisionKind::GoTo(Destination::Building(source)),
            ]);
        set_router(&mut f, truck, source, Some(RouterError::LocatingVehicle));
        let failed = 2 * FREIGHT_MATCH_INTERVAL;
        f.update(failed);
        f.update(failed + 30 * TICKS_PER_MINUTE);
        drain_commands(&mut f);
        let human = &f.ctx.g.world().humans[driver];
        assert!(
            !matches!(human.decision.kind, HumanDecisionKind::MultiStack(_)),
            "the stranded return is abandoned"
        );
        assert_ne!(human.router.selected_vehicle(), Some(truck));
        assert!(human.router.target_dest.is_none());

        // Once the driver is back at the depot, the fleet takes the next batch.
        f.ctx.g.world_mut_unchecked().humans[driver].location = Location::Building(source);
        let delivered = f.hand(f.destination);
        f.ctx
            .g
            .write::<Market>()
            .produce(f.destination.soul, Fixture::coal(), -delivered);
        f.seed(16, 0);
        f.update(failed + 31 * TICKS_PER_MINUTE);
        let book = f.ctx.g.read::<CargoBook>();
        assert_eq!(book.loads.len(), 1);
        assert!(book.loads.values().all(|load| load.source() == source));
    }

    #[test]
    fn e2fix_outbound_router_errors_retry_before_cancelling_then_suspend_the_route() {
        use crate::map_dynamic::ParkingReserveError;

        let mut f = Fixture::new();
        let terminal = station(&mut f, false);
        let SoulID::FreightStation(station) = terminal.soul else {
            unreachable!()
        };
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.manifest_units(), 16);
        let (truck, destination) = (f.truck, f.destination.building);
        let full = Some(RouterError::ReservingParkingSpot(
            ParkingReserveError::NoSpotFoundAfterSearch,
        ));
        let start = FREIGHT_MATCH_INTERVAL + 1;
        set_router(&mut f, truck, destination, full);
        f.update(start);
        f.update(start + TICKS_PER_MINUTE);
        assert_eq!(f.manifest_units(), 16, "a full car park is retried");
        set_router(&mut f, truck, destination, None);
        f.update(start + 2 * TICKS_PER_MINUTE);
        set_router(&mut f, truck, destination, full);
        f.update(start + 3 * TICKS_PER_MINUTE);
        f.update(start + 6 * TICKS_PER_MINUTE);
        assert_eq!(
            f.manifest_units(),
            16,
            "a cleared error restarts the window"
        );
        f.update(start + 30 * TICKS_PER_MINUTE);
        assert_eq!((f.manifest_units(), f.hand(f.source)), (0, 16));

        // The failing route is not reserved again. The plant still needs the
        // batch, so it stays at the mine instead of leaving by rail.
        set_router(&mut f, truck, destination, None);
        f.update(FREIGHT_MATCH_INTERVAL + 31 * TICKS_PER_MINUTE);
        assert_eq!(
            (f.manifest_units(), f.reserved(), f.hand(f.source)),
            (0, 0, 16)
        );
        assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 0);
    }

    #[test]
    fn e2fix_demolition_writes_off_on_hand_queued_and_loaded_freight_once() {
        for destination_first in [true, false] {
            let mut f = Fixture::new();
            // More than the plant takes, so some stays on hand at the mine.
            let seeded = plant_target(&f) + 16;
            f.seed(seeded, 1);
            f.update(FREIGHT_MATCH_INTERVAL);
            assert_eq!(f.manifest_units(), mine_policy().capacity);
            assert!(f.reserved() > 0 && f.hand(f.source) > 0);
            let (mut first, mut second) = (f.destination.building, f.source.building);
            if !destination_first {
                std::mem::swap(&mut first, &mut second);
            }
            for building in [first, second] {
                f.ctx.apply(&[WorldCommand::MapRemoveBuilding(building)]);
                f.ctx.tick_fast();
            }
            f.ctx.tick_fast();
            let losses = f
                .ctx
                .g
                .read::<Market>()
                .seller_losses()
                .get(&Fixture::coal())
                .copied();
            assert_eq!(
                losses,
                Some(seeded as u64 + 1),
                "destination first: {destination_first}"
            );
        }
    }

    #[test]
    fn e2fix_exports_skip_a_closer_station_without_road_access() {
        let mut f = Fixture::new();
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(f.destination.building)]);
        let far = station(&mut f, true);
        let near = station_at(&mut f, vec2(330.0, 200.0), None);
        let SoulID::FreightStation(near_id) = near.soul else {
            unreachable!()
        };
        f.ctx
            .g
            .write::<Market>()
            .produce(f.source.soul, Fixture::coal(), 16);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.ctx.g.get(near_id).unwrap().f.waiting_cargo, 0);
        let book = f.ctx.g.read::<CargoBook>();
        assert_eq!(
            book.manifest_for(f.truck).map(TruckLoad::destination),
            Some(far.building)
        );
    }

    #[test]
    fn e2fix_dispatch_serves_a_stocked_out_destination_before_an_older_route() {
        let mut f = Fixture::with_companies("flour-factory", "bakery");
        let terminal = station(&mut f, true);
        let flour = ItemID::new("flour");
        {
            let mut market = f.ctx.g.write::<Market>();
            market.produce(f.source.soul, flour, 32);
            let mut book = f.ctx.g.write::<CargoBook>();
            for (destination, reserved_at) in [(terminal, 0), (f.destination, 1)] {
                book.reserve(
                    &mut market,
                    Shipment {
                        source: f.source,
                        destination,
                        quantity: ItemQuantity {
                            item: flour,
                            units: 16,
                        },
                        reserved_at,
                    },
                );
            }
        }
        f.update(FREIGHT_MATCH_INTERVAL);
        let book = f.ctx.g.read::<CargoBook>();
        assert_eq!(
            book.manifest_for(f.truck).map(TruckLoad::destination),
            Some(f.destination.building)
        );
    }

    #[test]
    fn e2fix_cancelled_loads_are_not_counted_as_delivered_volume() {
        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        f.update(FREIGHT_MATCH_INTERVAL + DELIVERY_EXPIRY);
        assert_eq!((f.manifest_units(), f.hand(f.source)), (0, 16));
        let metrics = f.ctx.g.read::<DailyEconomyMetrics>();
        let day = metrics.days.get(&0);
        assert_eq!(day.and_then(|day| day.loads).unwrap_or(0), 0);
        assert_eq!(day.and_then(|day| day.load_units).unwrap_or(0), 0);
    }

    #[test]
    fn e2fix_route_cache_follows_a_changed_building_road_connection() {
        let mut f = Fixture::new();
        f.ctx
            .build_roads(&[vec3(700.0, 100.0, 0.0), vec3(900.0, 100.0, 0.0)]);
        let isolated = road_along(&f, None, Some(100.0));
        let isolated = if f.ctx.g.map().roads()[isolated].points().first().x >= 700.0 {
            isolated
        } else {
            let map = f.ctx.g.map();
            map.roads()
                .iter()
                .find(|(_, road)| road.points().first().x >= 700.0)
                .unwrap()
                .0
        };
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.manifest_units(), 16);
        f.ctx.g.map_mut().buildings[f.destination.building].connected_road = Some(isolated);
        f.update(FREIGHT_MATCH_INTERVAL + 1);
        assert_eq!((f.manifest_units(), f.hand(f.source)), (0, 16));
    }

    #[test]
    fn e2fix_balanced_depot_exports_unused_stock_before_locally_demanded_stock() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        warehouse_driver(&mut f, depot);
        let terminal = station(&mut f, true);
        let target = plant_target(&f);
        f.seed(0, target);
        let flour = ItemID::new("flour");
        {
            let mut market = f.ctx.g.write::<Market>();
            market.produce(depot.soul, Fixture::coal(), 40);
            market.produce(depot.soul, flour, 20);
        }
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(incoming(&f, terminal.building, Fixture::coal()), 0);
        assert_eq!(incoming(&f, terminal.building, flour), 16);
    }

    #[test]
    fn e2fix_depot_supplies_a_consumer_when_the_closer_producer_is_empty() {
        let mut f = Fixture::new();
        let north = f.ctx.g.map().buildings()[f.source.building].connected_road;
        let depot = place_warehouse_on(
            &mut f,
            GoodsCompanyID::new("small-depot").prototype(),
            vec2(310.0, 142.0),
            north,
        );
        warehouse_driver(&mut f, depot);
        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, Fixture::coal(), 48);
        let amount = fixture_input(&f.ctx.g, f.destination, Fixture::coal())
            .unwrap()
            .amount;
        f.seed(0, amount as i32);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert!(incoming(&f, f.destination.building, Fixture::coal()) > 0);
    }

    #[test]
    fn e2fix_only_staffed_depots_take_producer_stock() {
        let mut f = Fixture::new();
        let depot = GoodsCompanyID::new("small-depot").prototype();
        // The idle depot is closer to the mine, and both sit upstream of the plant.
        let idle = place_warehouse(&mut f, depot, vec2(225.0, 450.0));
        let staffed = place_warehouse(&mut f, depot, vec2(150.0, 450.0));
        warehouse_driver(&mut f, staffed);
        let target = plant_target(&f);
        f.seed(100, target);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert!(f
            .ctx
            .g
            .read::<CargoBook>()
            .stock_for(idle.building)
            .is_empty());
        assert!(incoming(&f, staffed.building, Fixture::coal()) > 0);
    }

    #[test]
    fn e2fix_policy_withdrawal_is_not_a_late_delivery() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        let (_, truck) = warehouse_driver(&mut f, depot);
        station(&mut f, true);
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(f.destination.building)]);
        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, Fixture::coal(), 64);
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: depot.building,
            policy: WarehousePolicy::ExportSurplus,
        }]);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_some());
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: depot.building,
            policy: WarehousePolicy::ReserveLocally,
        }]);
        assert!(f.ctx.g.read::<CargoBook>().manifest_for(truck).is_none());
        let metrics = f.ctx.g.read::<DailyEconomyMetrics>();
        let day = metrics.days.get(&0);
        assert_eq!(day.and_then(|day| day.loads).unwrap_or(0), 0);
        assert_eq!(day.and_then(|day| day.late_loads).unwrap_or(0), 0);
    }

    #[test]
    fn e2fix_a_full_depot_still_takes_a_missing_locally_used_item() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        warehouse_driver(&mut f, depot);
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: depot.building,
            policy: WarehousePolicy::ReserveLocally,
        }]);
        f.seed(200, 0);
        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, ItemID::new("goods"), 48);
        for step in 1..=4 {
            f.update(step * FREIGHT_MATCH_INTERVAL);
            if incoming(&f, depot.building, Fixture::coal()) > 0 {
                break;
            }
            let (driver, truck, plant) = (f.driver, f.truck, f.destination.building);
            unload_fixture(&mut f, driver, truck, plant);
        }
        assert_eq!(incoming(&f, depot.building, Fixture::coal()), 16);
    }

    #[test]
    fn e2fix_an_empty_export_depot_stages_a_terminal_only_route() {
        let mut f = Fixture::new();
        let west = road_along(&f, Some(100.0), None);
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(f.destination.building)]);
        station(&mut f, true);
        let depot = place_warehouse_on(
            &mut f,
            GoodsCompanyID::new("small-depot").prototype(),
            vec2(142.0, 315.0),
            Some(west),
        );
        warehouse_driver(&mut f, depot);
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: depot.building,
            policy: WarehousePolicy::ExportSurplus,
        }]);
        f.ctx
            .g
            .write::<Market>()
            .produce(f.source.soul, Fixture::coal(), 32);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(incoming(&f, depot.building, Fixture::coal()), 32);
    }

    #[test]
    fn e2fix_warehouse_policy_latch_and_loads_survive_a_save() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        warehouse_driver(&mut f, depot);
        station(&mut f, true);
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: depot.building,
            policy: WarehousePolicy::ExportSurplus,
        }]);
        let target = plant_target(&f);
        // One truckload stays for the plant; the rest is surplus for the depot.
        f.seed(mine_policy().capacity as i32 + 24, target);
        f.update(FREIGHT_MATCH_INTERVAL);
        let resumed: Simulation = Bincode::decode(&Bincode::encode(&f.ctx.g).unwrap()).unwrap();
        assert_eq!(f.ctx.g.hashes(), resumed.hashes());
        let (book, saved) = (f.ctx.g.read::<CargoBook>(), resumed.read::<CargoBook>());
        let SoulID::GoodsCompany(company) = depot.soul else {
            unreachable!()
        };
        assert!(book.restocking.contains(&company));
        assert_eq!(book.policies[&company], WarehousePolicy::ExportSurplus);
        assert!(!book.loads.is_empty());
        assert_eq!(book.policies, saved.policies);
        assert_eq!(book.restocking, saved.restocking);
        assert!(book.loads.keys().eq(saved.loads.keys()));
        assert!(book.shipments.keys().eq(saved.shipments.keys()));
    }

    #[test]
    fn e2fix_a_refused_unload_cancels_the_load_with_its_reason() {
        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let (driver, truck, destination) = (f.driver, f.truck, f.destination.building);
        let token = f.token();
        f.ctx
            .g
            .write::<CargoBook>()
            .loads
            .get_mut(&token)
            .unwrap()
            .used_truck = true;
        // The driver is still at the depot, so the unload is refused.
        assert_eq!(
            complete_delivery(&mut f.ctx.g, token, driver, truck, destination),
            Err(DeliveryRejection::DriverNotAtDestination)
        );
        assert_eq!(f.manifest_units(), 16);
        deliver_load(&mut f.ctx.g, token, driver, truck, destination);
        assert_eq!(
            (f.manifest_units(), f.hand(f.source), f.hand(f.destination)),
            (0, 16, 1)
        );
        assert_eq!(
            complete_delivery(&mut f.ctx.g, token, driver, truck, destination),
            Err(DeliveryRejection::UnknownLoad)
        );
        let book = f.ctx.g.read::<CargoBook>();
        let day = book.freight_day(0).unwrap();
        assert_eq!(
            (
                day.refused_unloads,
                day.cancelled_loads,
                day.delivered_loads
            ),
            (1, 1, 0)
        );
    }

    #[test]
    fn l1_activity_queued_late_cargo_refusal_then_driver_recovery() {
        let legacy = l1_cargo_refusal_trace(true);
        let indexed = l1_cargo_refusal_trace(false);
        for (phase, (expected, actual)) in [
            "decision",
            "unload refusal",
            "queued recovery",
            "itinerary update",
        ]
        .into_iter()
        .zip(legacy.into_iter().zip(indexed))
        {
            assert_eq!(expected, actual, "after {phase}");
        }
    }

    fn l1_cargo_refusal_trace(full_scan: bool) -> [BTreeMap<String, u64>; 4] {
        use crate::map_dynamic::{itinerary_update, Destination, RoutingStep};
        use crate::souls::human::{update_decision_system, HumanDecisionKind};
        use crate::HumanWork;

        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let load = f.token();
        let due = f.ctx.g.read::<CargoBook>().loads[&load].due_at;
        f.at_tick(due + 1);
        let sim = &mut f.ctx.g;
        if full_scan {
            sim.world.use_legacy_human_scan();
        }
        let (driver, truck, destination) = (f.driver, f.truck, f.destination.building);
        let pos = sim.get(truck).unwrap().trans.pos;
        let human = &mut sim.world.humans[driver];
        human.location = Location::Vehicle(truck);
        human.router.use_vehicle(Some(truck));
        human
            .router
            .set_test_route(Destination::Outside(pos), vec![RoutingStep::WalkTo(pos)]);
        human.it = Itinerary::simple(vec![pos]);
        human.decision.kind = HumanDecisionKind::DeliverLoad { destination, load };
        human.decision.wait = 0;

        let (world, resources) = sim.world_res();
        let mut candidates = Vec::new();
        if !full_scan {
            world.collect_active(HumanWork::Itinerary, &mut candidates);
            assert_eq!(candidates, [driver]);
        }
        update_decision_system(world, resources);
        assert!(sim.read::<CargoBook>().manifest_for(truck).is_some());
        let decision = sim.hashes();

        // The delivery callback refuses the unload and queues a second human
        // callback. Inspect the state before that recovery is allowed to run.
        assert!(ParCommandBuffer::<HumanEnt>::apply(sim));
        assert!(sim.read::<CargoBook>().manifest_for(truck).is_none());
        assert_eq!(sim.get(driver).unwrap().location, Location::Vehicle(truck));
        assert_eq!(
            sim.read::<Market>().capital(f.source.soul, Fixture::coal()),
            16
        );
        let refusal = sim.hashes();

        assert!(ParCommandBuffer::<HumanEnt>::apply(sim));
        let human = sim.get(driver).unwrap();
        assert_eq!(human.location, Location::Outside);
        assert_eq!(human.router.target_dest, None);
        assert!(human.collider.is_some());
        assert!(human.it.is_inert());
        let book = sim.read::<CargoBook>();
        let day = book.freight_day(0).unwrap();
        assert_eq!(
            (
                day.refused_unloads,
                day.cancelled_loads,
                day.delivered_loads
            ),
            (1, 1, 0)
        );
        drop(book);
        if !full_scan {
            sim.world
                .collect_active(HumanWork::Itinerary, &mut candidates);
            assert!(candidates.is_empty());
            sim.world
                .collect_active(HumanWork::Outside, &mut candidates);
            assert_eq!(candidates, [driver]);
        }
        let recovery = sim.hashes();
        let (world, resources) = sim.world_res();
        itinerary_update(world, resources);
        [decision, refusal, recovery, sim.hashes()]
    }

    #[test]
    fn e2fix_reachability_runs_no_route_search_and_shift_distances_are_cached() {
        let mut f = Fixture::new();
        f.seed(100, 1);
        let target = plant_target(&f);
        {
            let map = f.ctx.g.map();
            let mut book = f.ctx.g.write::<CargoBook>();
            assert!(book.physical_route(&map, f.source, f.destination, 0));
            assert_eq!(book.routes.searches, 0);
        }
        for step in 1..=4 {
            f.update(step * FREIGHT_MATCH_INTERVAL);
        }
        assert!(f.reserved() > 0 && f.reserved() < target as u32);
        assert_eq!(f.ctx.g.read::<CargoBook>().routes.searches, 2);

        // A road edit relabels the components on the next check.
        let road = f.ctx.g.map().buildings()[f.destination.building]
            .connected_road
            .unwrap();
        f.ctx.apply(&[WorldCommand::MapRemoveRoad(road)]);
        f.update(5 * FREIGHT_MATCH_INTERVAL + 1);
        assert_eq!((f.manifest_units(), f.reserved()), (0, 0));
        assert_eq!(f.ctx.g.read::<CargoBook>().routes.searches, 2);
    }

    #[test]
    fn e2fix_receipts_separate_truck_unloads_from_station_imports() {
        let mut f = Fixture::new();
        f.seed(16, 0);
        f.update(FREIGHT_MATCH_INTERVAL);
        let (driver, truck, at) = (f.driver, f.truck, f.destination.building);
        unload_fixture(&mut f, driver, truck, at);
        let terminal = station(&mut f, false);
        let import = [ItemQuantity {
            item: Fixture::coal(),
            units: 3,
        }];
        {
            let (_, resources) = f.ctx.g.world_res();
            let mut book = resources.write::<CargoBook>();
            unload_import(&mut book, resources, terminal, f.destination, &import, 0);
        }
        let book = f.ctx.g.read::<CargoBook>();
        let day = book.freight_day(0).unwrap();
        assert_eq!(
            day.receipt(f.destination.soul, Fixture::coal()),
            FreightReceipt {
                road_units: 16,
                station_units: 3
            }
        );
        assert_eq!((day.delivered_loads, day.delivered_units), (1, 16));
    }

    #[test]
    fn e2fix_a_real_empty_return_finishes_without_recovery() {
        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let token = f.token();
        for _ in 0..20_000 {
            f.ctx.tick_fast();
            if !f.ctx.g.read::<CargoBook>().loads.contains_key(&token) {
                break;
            }
        }
        assert_eq!(f.hand(f.destination), 17);
        assert!(f.ctx.g.read::<CargoBook>().returning.contains_key(&f.truck));
        for _ in 0..20_000 {
            f.ctx.tick_fast();
            if f.ctx.g.read::<CargoBook>().returning.is_empty() {
                break;
            }
        }
        assert!(f.ctx.g.read::<CargoBook>().returning.is_empty());
        let human = &f.ctx.g.world().humans[f.driver];
        assert_eq!(human.location, Location::Building(f.source.building));
        assert!(f.ctx.g.get(f.truck).unwrap().vehicle.state.is_parked());
    }

    #[test]
    fn whfix_new_games_truck_zoned_goods_and_stores_wait_for_the_delivery() {
        assert!(
            TestCtx::new_fast().g.read::<CargoBook>().growable_freight,
            "new games truck zoned industry's output"
        );
        let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
        let goods = ItemID::new("goods");
        let SoulID::GoodsCompany(industry) = f.source.soul else {
            unreachable!()
        };
        // The factory finishes a batch. The store may only receive it by truck.
        f.ctx.g.world_mut_unchecked().companies[industry]
            .comp
            .progress = 1.0;
        for _ in 0..2 * FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
        }
        let store_goods = |f: &Fixture| f.ctx.g.read::<Market>().capital(f.destination.soul, goods);
        assert_eq!(store_goods(&f), 0, "no virtual hand-off to the store");
        let loaded = f.manifest_units();
        assert!(loaded > 0, "the factory's truck carries the batch");
        assert_eq!(incoming(&f, f.destination.building, goods), loaded);
    }

    #[test]
    fn whfix_a_staffed_depot_takes_a_zoned_factory_surplus_once_stores_are_stocked() {
        let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
        let goods = ItemID::new("goods");
        let depot = warehouse(&mut f, "small-depot");
        warehouse_driver(&mut f, depot);
        let proto = GoodsCompanyID::new("industrial-l1").prototype();
        let recipe = proto.recipe.as_ref().unwrap();
        let output = &recipe.production[0];
        assert_eq!(output.id, goods);
        // The factory stops producing at this stock.
        let full = output.amount * (recipe.storage_multiplier + 1);
        let policy = DispatchPolicy::for_prototype(proto).unwrap();
        let input = fixture_input(&f.ctx.g, f.destination, goods).unwrap();
        {
            let mut market = f.ctx.g.write::<Market>();
            market.produce(f.source.soul, goods, full);
            market.receive_freight(f.destination.soul, goods, input.target(policy));
        }
        f.update(FREIGHT_MATCH_INTERVAL);
        assert!(
            incoming(&f, depot.building, goods) >= policy.minimum,
            "a full factory hands a dispatchable batch to the depot (holds {full})",
        );
        assert!(
            f.manifest_units() >= policy.minimum,
            "the batch leaves at once"
        );
    }

    #[test]
    fn whfix_depot_activity_names_why_it_is_idle() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        let SoulID::GoodsCompany(id) = depot.soul else {
            unreachable!()
        };
        let activity = |f: &Fixture, id| f.ctx.g.read::<CargoBook>().warehouse_activity(id);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(activity(&f, id), Some(WarehouseActivity::NoWorkers));

        warehouse_driver(&mut f, depot);
        f.update(2 * FREIGHT_MATCH_INTERVAL);
        assert_eq!(activity(&f, id), Some(WarehouseActivity::WaitingForSupply));

        let target = plant_target(&f);
        f.seed(100, target);
        f.update(3 * FREIGHT_MATCH_INTERVAL);
        assert_eq!(activity(&f, id), Some(WarehouseActivity::Moving));

        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        let SoulID::GoodsCompany(id) = depot.soul else {
            unreachable!()
        };
        warehouse_driver(&mut f, depot);
        let target = plant_target(&f);
        f.seed(0, target);
        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, Fixture::coal(), 48);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(
            activity(&f, id),
            Some(WarehouseActivity::NothingBelowWatermark)
        );

        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, Fixture::coal(), -48);
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(f.source.building)]);
        f.ctx.tick_fast();
        f.update(2 * FREIGHT_MATCH_INTERVAL);
        assert_eq!(activity(&f, id), Some(WarehouseActivity::NoSupplierInReach));
    }

    #[test]
    fn whfix_a_store_waits_for_its_truck_instead_of_importing_then_reorders() {
        let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
        let terminal = import_station(&mut f);
        let goods = ItemID::new("goods");
        let store = f.destination;
        let SoulID::GoodsCompany(store_id) = store.soul else {
            unreachable!()
        };
        {
            let mut market = f.ctx.g.write::<Market>();
            market.receive_freight(store.soul, goods, 2);
            market.produce(f.source.soul, goods, 16);
        }
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.manifest_units(), 16, "a truck is on its way to the store");

        // The store uses its last goods while the truck drives.
        f.ctx.g.world_mut_unchecked().companies[store_id]
            .comp
            .progress = 1.0;
        for _ in 0..10 {
            f.ctx.tick_fast();
        }
        let market = f.ctx.g.read::<Market>();
        assert_eq!(market.capital(store.soul, goods), 0, "no station import");
        assert!(market.inner()[&goods].buy_order(store.soul).is_none());
        drop(market);

        // Without the truck, the store orders an import and waits for it.
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(f.source.building)]);
        for _ in 0..=FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
        }
        assert!(f.ctx.g.read::<CargoBook>().manifest_for(f.truck).is_none());
        assert_eq!(f.ctx.g.read::<Market>().capital(store.soul, goods), 0);
        let SoulID::FreightStation(station) = terminal.soul else {
            unreachable!()
        };
        assert!(
            f.ctx.g.get(station).unwrap().f.wanted_cargo > 0,
            "the station orders the store's goods once no truck is coming",
        );
    }

    /// Ticks until the driver rides the loaded truck on its planned route, and
    /// returns where that route ends.
    fn drive_off(f: &mut Fixture) -> geom::Vec3 {
        for _ in 0..5_000 {
            f.ctx.tick_fast();
            let world = f.ctx.g.world();
            let riding = world.humans[f.driver].location == Location::Vehicle(f.truck);
            if let Some(end) = world.vehicles[f.truck].it.end_pos().filter(|_| riding) {
                return end;
            }
        }
        panic!("the driver never drove off in the truck");
    }

    fn suspend(f: &mut Fixture, from: Endpoint, to: Endpoint, now: u64) {
        let map = f.ctx.g.map();
        f.ctx
            .g
            .write::<CargoBook>()
            .suspend_route(&map, from, to, now);
    }

    /// Unloads at the plant and leaves the driver there, so the empty return
    /// is still being tracked.
    fn unload_and_stay(f: &mut Fixture) -> u64 {
        let (driver, truck, destination) = (f.driver, f.truck, f.destination.building);
        let token = f.token();
        f.ctx
            .g
            .write::<CargoBook>()
            .loads
            .get_mut(&token)
            .unwrap()
            .used_truck = true;
        {
            let human = &mut f.ctx.g.world_mut_unchecked().humans[driver];
            human.location = Location::Building(destination);
            human.router.use_vehicle(Some(truck));
        }
        assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, destination).is_ok());
        assert!(f.ctx.g.read::<CargoBook>().returning.contains_key(&truck));
        elapsed_ticks(&f.ctx.g.resources).0
    }

    #[test]
    fn e2fix2_a_real_failed_reroute_gives_up_after_the_retry_window() {
        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let token = f.token();
        let end = drive_off(&mut f);

        // A road edit leaves the truck on a fragment that cannot reach the
        // plant, while the mine and the plant still reach each other.
        f.ctx
            .build_roads(&[vec3(700.0, 700.0, 0.0), vec3(900.0, 700.0, 0.0)]);
        let fragment = {
            let map = f.ctx.g.map();
            let (_, road) = map
                .roads()
                .iter()
                .find(|(_, road)| road.points().first().x >= 700.0)
                .unwrap();
            road.points().first().lerp(road.points().last(), 0.5)
        };
        {
            let truck = &mut f.ctx.g.world_mut_unchecked().vehicles[f.truck];
            truck.trans.pos = fragment;
            truck.it = Itinerary::wait_for_reroute(PathKind::Vehicle, end);
        }
        let mut retried = false;
        for _ in 0..ROUTING_RETRY_WINDOW + 2 * FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
            if !f.ctx.g.read::<CargoBook>().loads.contains_key(&token) {
                break;
            }
            retried |= f.ctx.g.world().vehicles[f.truck]
                .it
                .is_wait_for_reroute()
                .is_some_and(|wait| wait > 0);
        }
        assert!(retried, "the router really tried and failed to reroute");
        let book = f.ctx.g.read::<CargoBook>();
        assert!(
            !book.loads.contains_key(&token),
            "a reroute that keeps failing gives up within the retry window"
        );
        assert!(book.suspended.contains_key(&(f.source, f.destination)));
    }

    #[test]
    fn e2fix2_recovering_an_empty_truck_leaves_the_drivers_other_trip_alone() {
        use crate::souls::human::HumanDecisionKind;

        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let started = unload_and_stay(&mut f);
        let elsewhere = f.ctx.build_house_near(vec2(100.0, 380.0));
        {
            let human = &mut f.ctx.g.world_mut_unchecked().humans[f.driver];
            let personal = human.router.personal_car;
            human.router.use_vehicle(personal);
            human.router.go_to(Destination::Building(elsewhere));
            human.decision.kind = HumanDecisionKind::GoTo(Destination::Building(elsewhere));
        }
        // The return outlasts a delivery window, so the truck is recovered.
        f.update(started + DELIVERY_EXPIRY);
        drain_commands(&mut f);
        assert!(f.ctx.g.read::<CargoBook>().returning.is_empty());
        let human = &f.ctx.g.world().humans[f.driver];
        assert_eq!(
            human.router.target_dest,
            Some(Destination::Building(elsewhere)),
            "an unrelated trip is not cancelled"
        );
        assert!(matches!(
            human.decision.kind,
            HumanDecisionKind::GoTo(Destination::Building(id)) if id == elsewhere
        ));
    }

    #[test]
    fn finalsim_replacing_a_truck_parked_away_leaves_the_drivers_other_trip_alone() {
        use crate::souls::human::HumanDecisionKind;

        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let started = unload_and_stay(&mut f);
        // The empty truck is parked at the plant, so recovery replaces it at the depot.
        let plant_door = f.ctx.g.map().buildings()[f.destination.building].door_pos;
        f.ctx.g.world_mut_unchecked().vehicles[f.truck].trans.pos = plant_door;
        let elsewhere = f.ctx.build_house_near(vec2(100.0, 380.0));
        {
            let human = &mut f.ctx.g.world_mut_unchecked().humans[f.driver];
            let personal = human.router.personal_car;
            human.router.use_vehicle(personal);
            human.router.go_to(Destination::Building(elsewhere));
            human.decision.kind = HumanDecisionKind::GoTo(Destination::Building(elsewhere));
        }
        f.update(started + DELIVERY_EXPIRY);
        drain_commands(&mut f);
        drain_commands(&mut f);
        let SoulID::GoodsCompany(mine) = f.source.soul else {
            unreachable!()
        };
        let fleet = f.ctx.g.world().companies[mine].comp.trucks.clone();
        assert_ne!(fleet, vec![f.truck], "the truck was replaced");
        let human = &f.ctx.g.world().humans[f.driver];
        assert_eq!(
            human.router.target_dest,
            Some(Destination::Building(elsewhere)),
            "an unrelated trip is not cancelled"
        );
        assert!(
            matches!(
                human.work.as_ref().unwrap().kind,
                WorkKind::Driver { truck, .. } if fleet.contains(&truck)
            ),
            "the driver keeps the job with the replacement truck"
        );
    }

    #[test]
    fn e2fix2_a_return_without_its_driver_brings_the_truck_back_to_the_depot() {
        for reason in ["despawned", "reassigned"] {
            let mut f = Fixture::new();
            f.seed(16, 1);
            f.update(FREIGHT_MATCH_INTERVAL);
            let started = unload_and_stay(&mut f);
            let (driver, truck) = (f.driver, f.truck);
            let plant_door = f.ctx.g.map().buildings()[f.destination.building].door_pos;
            // The empty truck is parked at the plant.
            f.ctx.g.world_mut_unchecked().vehicles[truck].trans.pos = plant_door;
            match reason {
                "despawned" => {
                    crate::souls::lifecycle::despawn_human(&mut f.ctx.g, driver);
                    drain_commands(&mut f);
                    assert!(f.ctx.g.get(driver).is_none());
                }
                "reassigned" => {
                    f.ctx.g.world_mut_unchecked().humans[driver]
                        .work
                        .as_mut()
                        .unwrap()
                        .kind = WorkKind::Worker;
                }
                _ => unreachable!(),
            }
            f.update(started + 1);
            drain_commands(&mut f);
            assert!(f.ctx.g.read::<CargoBook>().returning.is_empty(), "{reason}");
            let SoulID::GoodsCompany(mine) = f.source.soul else {
                unreachable!()
            };
            let depot_door = f.ctx.g.map().buildings()[f.source.building].door_pos;
            let fleet = f.ctx.g.world().companies[mine].comp.trucks.clone();
            assert_eq!(fleet.len(), 1, "{reason}");
            let at_depot = f.ctx.g.get(fleet[0]).is_some_and(|vehicle| {
                vehicle.trans.pos.distance(depot_door) <= crate::map_dynamic::NEAR_PARKING_RADIUS
            });
            assert!(at_depot, "{reason}: the truck is back at its depot");
        }
    }

    #[test]
    fn e2fix2_a_suspended_route_keeps_the_consumers_stock_back() {
        let mut f = Fixture::new();
        let terminal = station(&mut f, false);
        let SoulID::FreightStation(station_id) = terminal.soul else {
            unreachable!()
        };
        let policy = mine_policy();
        // The plant could take this much if its route were open.
        let wanted = plant_target(&f) as u32 - 1;
        let surplus = 16;
        f.seed((wanted + policy.capacity + surplus) as i32, 1);
        let (source, destination) = (f.source, f.destination);
        suspend(&mut f, source, destination, 0);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.reserved(), 0, "a suspended route is not reserved");
        assert_eq!(
            f.ctx.g.get(station_id).unwrap().f.waiting_cargo,
            surplus,
            "only stock an open route would leave over is exported"
        );
        assert_eq!(f.hand(f.source) as u32, wanted + policy.capacity);

        // A depot keeps a locally used item below its high watermark.
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        warehouse_driver(&mut f, depot);
        let terminal = station(&mut f, true);
        let target = plant_target(&f);
        f.seed(0, target);
        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, Fixture::coal(), 40);
        let destination = f.destination;
        suspend(&mut f, depot, destination, 0);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(incoming(&f, terminal.building, Fixture::coal()), 0);
        assert_eq!(f.hand(depot), 40);
    }

    #[test]
    fn e2fix2_setting_the_default_policy_explicitly_keeps_queued_exports() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        let (driver, _) = warehouse_driver(&mut f, depot);
        // The driver is away, so the export stays queued at the dock.
        f.ctx.g.world_mut_unchecked().humans[driver].location = Location::Outside;
        let terminal = station(&mut f, true);
        let flour = ItemID::new("flour");
        f.ctx.g.write::<Market>().produce(depot.soul, flour, 40);
        f.update(FREIGHT_MATCH_INTERVAL);
        let queued = |f: &Fixture| {
            f.ctx
                .g
                .read::<CargoBook>()
                .shipments
                .values()
                .filter(|s| s.destination == terminal)
                .count()
        };
        assert_eq!(queued(&f), 1);
        let default = GoodsCompanyID::new("small-depot")
            .prototype()
            .storage
            .as_ref()
            .unwrap()
            .default_policy;
        f.ctx.apply(&[WorldCommand::SetWarehousePolicy {
            building: depot.building,
            policy: default,
        }]);
        assert_eq!(queued(&f), 1, "an unchanged policy withdraws nothing");
        assert_eq!(f.ctx.g.read::<Market>().capital(depot.soul, flour), 24);
    }

    fn update_sim(sim: &mut Simulation, tick: u64) {
        *sim.write::<GameTime>() = GameTime::new(Tick(INITIAL_TICK + tick));
        let (world, resources) = sim.world_res();
        cargo_update(world, resources);
    }

    #[test]
    fn finalsim_a_suspended_route_stays_suspended_after_a_save() {
        let mut f = Fixture::new();
        // Road building elsewhere advances the topology generation, which a save does not keep.
        f.ctx
            .build_roads(&[vec3(700.0, 700.0, 0.0), vec3(900.0, 700.0, 0.0)]);
        assert!(f.ctx.g.map().walking_topology_generation > 0);
        let wanted = plant_target(&f) as u32 - 1;
        f.seed((wanted + mine_policy().capacity) as i32, 1);
        let (source, destination) = (f.source, f.destination);
        suspend(&mut f, source, destination, 0);

        let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&f.ctx.g).unwrap()).unwrap();
        let held = |sim: &Simulation| {
            let map = sim.map();
            sim.write::<CargoBook>()
                .held_route(&map, source, destination, FREIGHT_MATCH_INTERVAL)
        };
        assert!(held(&f.ctx.g));
        assert!(held(&resumed), "the resumed run keeps the suspension");

        f.update(FREIGHT_MATCH_INTERVAL);
        update_sim(&mut resumed, FREIGHT_MATCH_INTERVAL);
        assert_eq!(
            f.reserved(),
            0,
            "the uninterrupted run holds the stock back"
        );
        assert_eq!(f.ctx.g.hashes(), resumed.hashes());
    }

    #[test]
    fn finalsim_a_road_edit_lifts_suspensions() {
        let mut f = Fixture::new();
        let (source, destination) = (f.source, f.destination);
        suspend(&mut f, source, destination, 0);
        // Building a new road is a topology edit that keeps both ends connected.
        f.ctx.apply(&[WorldCommand::MapMakeConnection {
            from: crate::map::MapProject::ground(vec3(700.0, 700.0, 0.0)),
            to: crate::map::MapProject::ground(vec3(900.0, 700.0, 0.0)),
            inter: None,
            pat: crate::map::LanePatternBuilder::default().build(),
        }]);
        let map = f.ctx.g.map();
        assert!(!f.ctx.g.write::<CargoBook>().held_route(
            &map,
            source,
            destination,
            FREIGHT_MATCH_INTERVAL
        ));
    }

    #[test]
    fn finalsim_queued_shipments_wait_for_the_match_after_a_load() {
        let mut f = Fixture::new();
        f.ctx.g.world_mut_unchecked().humans[f.driver].location = Location::Outside;
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert_eq!(f.reserved(), 16, "queued: the driver is away");
        // The driver loses the job between matches. The uninterrupted run only notices at the
        // next match, and so must the resumed one.
        f.ctx.g.world_mut_unchecked().companies[match f.source.soul {
            SoulID::GoodsCompany(id) => id,
            _ => unreachable!(),
        }]
        .comp
        .driver = None;
        let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&f.ctx.g).unwrap()).unwrap();
        f.update(FREIGHT_MATCH_INTERVAL + 1);
        update_sim(&mut resumed, FREIGHT_MATCH_INTERVAL + 1);
        assert_eq!(f.reserved(), 16);
        assert_eq!(f.ctx.g.hashes(), resumed.hashes());
    }

    #[test]
    fn finalsim_a_truck_that_stops_moving_releases_its_load() {
        let mut f = Fixture::new();
        f.seed(16, 1);
        f.update(FREIGHT_MATCH_INTERVAL);
        let token = f.token();
        drive_off(&mut f);
        let start = elapsed_ticks(&f.ctx.g.resources).0;
        // The truck stands still from here on, without a routing error.
        f.update(start + 1);
        f.update(start + LOAD_STALL_TICKS / 2);
        let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&f.ctx.g).unwrap()).unwrap();
        assert!(f.ctx.g.read::<CargoBook>().loads.contains_key(&token));
        f.update(start + 1 + LOAD_STALL_TICKS);
        update_sim(&mut resumed, start + 1 + LOAD_STALL_TICKS);
        assert_eq!(
            f.ctx.g.hashes(),
            resumed.hashes(),
            "the stall timer is saved"
        );
        drain_commands(&mut f);
        let book = f.ctx.g.read::<CargoBook>();
        assert!(
            !book.loads.contains_key(&token),
            "a stalled load is released"
        );
        assert!(
            book.suspended.is_empty(),
            "a jam does not suspend the route"
        );
        assert!(!book
            .inputs_in_transit(start + 1 + LOAD_STALL_TICKS)
            .contains(&(f.destination.soul, Fixture::coal())));
        drop(book);
        assert_eq!((f.hand(f.source), f.hand(f.destination)), (16, 1));
        assert!(f.ctx.g.world().humans[f.driver].location != Location::Vehicle(f.truck));
    }

    #[test]
    fn finalsim_a_store_imports_once_its_truck_is_overdue() {
        for overdue in [false, true] {
            let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
            import_station(&mut f);
            let goods = ItemID::new("goods");
            let store = f.destination;
            let SoulID::GoodsCompany(store_id) = store.soul else {
                unreachable!()
            };
            {
                let mut market = f.ctx.g.write::<Market>();
                market.receive_freight(store.soul, goods, 2);
                market.produce(f.source.soul, goods, 16);
            }
            f.update(FREIGHT_MATCH_INTERVAL);
            assert_eq!(f.manifest_units(), 16, "a truck is on its way to the store");
            if overdue {
                let now = elapsed_ticks(&f.ctx.g.resources).0;
                for load in f.ctx.g.write::<CargoBook>().loads.values_mut() {
                    load.due_at = now;
                }
            }
            f.ctx.g.world_mut_unchecked().companies[store_id]
                .comp
                .progress = 1.0;
            // The store uses its stock, then the next match decides on an import.
            for _ in 0..=FREIGHT_MATCH_INTERVAL {
                f.ctx.tick_fast();
            }
            assert_eq!(
                matches!(
                    f.ctx.g.read::<CargoBook>().import_status(store.soul),
                    Some(ImportStatus::Waiting { .. })
                ),
                overdue,
                "the station takes an order from the store only while its truck is overdue",
            );
        }
    }

    #[test]
    fn finalsim_held_stock_is_not_booked_as_shipped() {
        let mut f = Fixture::new();
        let depot = warehouse(&mut f, "small-depot");
        warehouse_driver(&mut f, depot);
        f.seed(0, 1);
        f.ctx
            .g
            .write::<Market>()
            .produce(depot.soul, Fixture::coal(), 40);
        let destination = f.destination;
        suspend(&mut f, depot, destination, 0);
        f.update(FREIGHT_MATCH_INTERVAL);
        let SoulID::GoodsCompany(id) = depot.soul else {
            unreachable!()
        };
        assert_eq!(f.hand(depot), 40);
        assert!(
            !f.ctx.g.read::<CargoBook>().is_restocking(id),
            "40 units held back on site are above the low watermark"
        );
    }

    #[test]
    fn finalsim_a_held_reorder_leaves_stock_for_an_open_consumer() {
        let mut f = Fixture::new();
        // A second plant on the plant's road, farther from the mine.
        let road = f.ctx.g.map().buildings()[f.destination.building].connected_road;
        let proto = GoodsCompanyID::new("coal-power-plant");
        let building = f
            .ctx
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(vec2(480.0, 467.0), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto),
                BuildingGen::CenteredDoor {
                    vertical_factor: 1.0,
                },
                None,
                road,
            )
            .unwrap();
        f.ctx.g.write::<BuildingInfos>().insert(building);
        let soul = company_soul(&mut f.ctx.g, building, proto).unwrap();
        let open = Endpoint { soul, building };
        let target = plant_target(&f);
        f.seed(target, 0);
        // The nearer plant's route failed and is cooling down.
        let (source, held) = (f.source, f.destination);
        suspend(&mut f, source, held, 0);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert!(
            incoming(&f, open.building, Fixture::coal()) > 0,
            "the reachable plant is served before stock is held for the suspended one"
        );
        assert_eq!(incoming(&f, held.building, Fixture::coal()), 0);
    }

    fn import_spend(f: &Fixture) -> i128 {
        f.ctx.g.read::<Budget>().total(BudgetCategory::TradeImport)
    }

    #[test]
    fn imports_a_market_pass_leaves_the_store_order_open() {
        let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
        station(&mut f, true);
        let goods = ItemID::new("goods");
        let store = f.destination.soul;
        let before = import_spend(&f);
        let (world, resources) = f.ctx.g.world_res();
        crate::economy::market_update(world, resources);
        let market = f.ctx.g.read::<Market>();
        assert_eq!(market.capital(store, goods), 0, "no goods land instantly");
        assert_eq!(market.inner()[&goods].buy_order(store).unwrap().qty, 2);
        drop(market);
        assert_eq!(import_spend(&f), before, "nothing is paid before delivery");
    }

    #[test]
    fn imports_need_a_road_route_to_a_station() {
        let mut f = Fixture::new();
        let terminal = station(&mut f, false);
        let SoulID::FreightStation(station) = terminal.soul else {
            unreachable!()
        };
        let before = import_spend(&f);
        for _ in 0..=2 * FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
        }
        assert_eq!(f.hand(f.destination), 0, "no import without a road route");
        assert_eq!(f.ctx.g.get(station).unwrap().f.wanted_cargo, 0);
        assert_eq!(import_spend(&f), before);
    }

    fn spawn_train(f: &mut Fixture) {
        let outside = {
            let map = f.ctx.g.map();
            map.buildings()[map.external_train_stations[0]]
                .obb
                .center()
                .z0()
        };
        let rail = f
            .ctx
            .g
            .map()
            .nearest_lane(outside, LaneKind::Rail, None)
            .unwrap();
        f.ctx.apply(&[WorldCommand::SpawnTrain {
            wagons: vec![prototypes::RollingStockID::new("locomotive")],
            lane: rail,
            dist: 0.0,
        }]);
    }

    /// A station inside the ring road with a train calling. The ring has no
    /// room for platform track, so the station counts as rail-served here;
    /// `imports_need_platform_track_with_a_rail_route_outside` builds real track.
    fn import_station(f: &mut Fixture) -> Endpoint {
        let terminal = station(f, true);
        spawn_train(f);
        let map = f.ctx.g.map();
        let mut book = f.ctx.g.write::<CargoBook>();
        book.routes.refresh(&map);
        book.routes.rail.insert(terminal.building, true);
        terminal
    }

    #[test]
    fn z3_fix_station_aggregates_and_bounds_transfers() {
        let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
        let station = import_station(&mut f);
        let buyer = f.destination;
        let item = ItemID::new("goods");
        let other = ItemID::new("flour");
        let mut buyers = slotmapd::SlotMap::<CompanyID, ()>::with_key();
        let mut book = CargoBook::default();
        let desk = book.imports.entry(station).or_default();
        for buyer in std::iter::once(buyer).chain((0..4).map(|_| Endpoint {
            soul: SoulID::GoodsCompany(buyers.insert(())),
            building: BuildingID::default(),
        })) {
            desk.place(ImportOrder {
                buyer,
                quantity: ItemQuantity {
                    item,
                    units: 1_073_741_823,
                },
                ordered_at: 0,
            });
        }
        let order = desk.orders[0];
        desk.place(ImportOrder {
            quantity: ItemQuantity {
                item: other,
                units: 1_000_000_000,
            },
            ..order
        });
        assert_eq!(desk.ordered(item), 5_368_709_115);
        assert_eq!(desk.on_rail_units(), 6_368_709_115);
        assert_eq!(desk.wanted_cargo(), u32::MAX);
        assert_eq!(book.unload_train(station.soul, TRAIN_CAPACITY), u32::MAX);
        assert_eq!(
            book.import_stock(station.building).values().sum::<u64>(),
            100
        );
        assert_eq!(book.unload_train(station.soul, u32::MAX), 2_073_741_720);
        assert_eq!(book.unload_train(station.soul, u32::MAX), 0);
        let stock = book.import_stock(station.building);
        assert_eq!(stock[&item], 5_368_709_115);
        assert_eq!(stock[&other], 1_000_000_000);

        let door = f.ctx.g.map().get(station.building).unwrap().door_pos;
        let truck = spawn_truck_at_depot(&mut f.ctx.g, door);
        book.station_trucks.insert(
            truck,
            StationTruck {
                station,
                trip: None,
            },
        );
        let (world, resources) = f.ctx.g.world_res();
        let map = resources.read::<Map>();
        book.routes.refresh(&map);
        dispatch_station_trucks(&mut book, world, resources, &map, 1);
        let trip = book.station_trucks[&truck].trip.as_ref().unwrap();
        assert_eq!(trip.buyer, buyer);
        assert_eq!(trip.items.len(), 1);
        assert_eq!(trip.items[0].item, item);
        assert_eq!(trip.items[0].units, STATION_TRUCK_CAPACITY);
        let stock = book.import_stock(station.building);
        assert_eq!(
            stock[&item],
            5_368_709_115 - u64::from(STATION_TRUCK_CAPACITY)
        );
    }

    #[test]
    fn z3_fix_live_road_and_station_reorders_follow_effective_cadence() {
        let goods = ItemID::new("goods");
        let run = |scale| {
            TEST_BATCH_TICKS_SCALE.set(scale);
            let mut road = Fixture::with_companies("industrial-l1", "commercial-l1");
            road.ctx
                .g
                .write::<Market>()
                .produce(road.source.soul, goods, 120);
            let (world, resources) = road.ctx.g.world_res();
            let map = resources.read::<Map>();
            let infos = resources.read::<BuildingInfos>();
            let mut book = resources.write::<CargoBook>();
            match_shipments(
                &mut book,
                world,
                resources,
                &map,
                &infos,
                0,
                &BTreeSet::new(),
            );
            let road_order = book.queued()[&(road.destination, goods)];

            let mut rail = Fixture::with_companies("industrial-l1", "commercial-l1");
            let station = import_station(&mut rail);
            let buyer = rail.destination;
            let door = rail.ctx.g.map().get(buyer.building).unwrap().door_pos;
            rail.ctx
                .g
                .write::<Market>()
                .buy(buyer.soul, door.xy(), goods, 2);
            rail.update(FREIGHT_MATCH_INTERVAL);
            let station_order = rail.ctx.g.read::<CargoBook>().imports[&station].ordered(goods);
            TEST_BATCH_TICKS_SCALE.set(1.0);
            (road_order, station_order)
        };
        // Keep both station targets below the truck limit so the rate can affect them.
        let (road, station) = run(4.0);
        let (faster_road, faster_station) = run(2.0);
        assert!(faster_road > road, "{faster_road} vs {road}");
        assert!(faster_station > station, "{faster_station} vs {station}");
        assert!(faster_station <= u64::from(STATION_TRUCK_CAPACITY));
    }

    #[test]
    fn imports_need_platform_track_with_a_rail_route_outside() {
        let mut f = Fixture::new();
        let road = road_along(&f, None, Some(500.0));
        let terminal = station_at(&mut f, vec2(370.0, 533.0), Some(road));
        let SoulID::FreightStation(id) = terminal.soul else {
            unreachable!()
        };
        let stop = train_stop(&f.ctx.g.get(id).unwrap().trans);
        let served = |f: &Fixture| {
            let map = f.ctx.g.map();
            let served =
                f.ctx
                    .g
                    .write::<CargoBook>()
                    .routes
                    .rail_served(&map, terminal.building, stop);
            served
        };
        assert!(!served(&f), "no platform track");
        {
            let mut map = f.ctx.g.map_mut();
            let platform = map.project(stop, 0.0, crate::map::ProjectFilter::ALL);
            // The outside track's junction with the main line.
            let junction = map.project(
                vec3(3036.0, 222.0, 0.0),
                10.0,
                crate::map::ProjectFilter::INTER,
            );
            // Leave the junction straight on from the outside track, then
            // curve round to the platform, since trains cannot take sharp turns.
            let bend = vec2(3036.0, stop.y);
            let rail = crate::map::LanePatternBuilder::new().rail(true).build();
            map.make_connection(junction, platform, Some(bend), &rail)
                .unwrap();
        }
        assert!(served(&f), "a spur to the outside track");
    }

    #[test]
    fn imports_see_an_authored_market_placed_after_matching() {
        use crate::map::{LanePatternBuilder, MapProject, RailMarketSite};
        use crate::world_command::CommandOutcomes;
        use crate::AuthoredTownOptions;

        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_town(AuthoredTownOptions::default()).unwrap();
        let light_road = LanePatternBuilder::new().build();
        let railway = LanePatternBuilder::new().rail(true).build();
        sim.apply_commands([
            &WorldCommand::MapMakeConnection {
                from: MapProject::ground(vec3(100.0, 625.0, 10.0)),
                to: MapProject::ground(vec3(1000.0, 625.0, 10.0)),
                inter: None,
                pat: light_road,
            },
            &WorldCommand::MapMakeConnection {
                from: MapProject::ground(vec3(100.0, 700.0, 10.0)),
                to: MapProject::ground(vec3(1000.0, 700.0, 10.0)),
                inter: None,
                pat: railway,
            },
        ]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0, 1]);
        let (road, lane, dir, spawn, platform) = {
            let map = sim.map();
            let road = map
                .lanes()
                .values()
                .find(|lane| lane.kind == LaneKind::Driving)
                .unwrap()
                .parent;
            let rail = map
                .lanes()
                .values()
                .find(|lane| {
                    lane.kind == LaneKind::Rail && lane.points.first().x < lane.points.last().x
                })
                .unwrap();
            let (spawn, dir) = rail.points.point_dir_along(200.0);
            (road, rail.id, dir, spawn, rail.points.point_along(600.0))
        };
        let station_proto = prototypes::FreightStationPrototypeID::new("freight-station");
        let store_proto = GoodsCompanyID::new("commercial-l1");
        let station_centre = (platform - dir * 75.0 + dir.perp_up() * 40.0).xy();
        let store_centre = vec2(350.0, 660.0);
        let build = |centre, direction, kind| {
            let building = sim
                .map_mut()
                .build_special_building(
                    &OBB::new(centre, direction, 12.0, 12.0),
                    kind,
                    BuildingGen::CenteredDoor {
                        vertical_factor: 1.0,
                    },
                    None,
                    Some(road),
                )
                .unwrap();
            sim.write::<BuildingInfos>().insert(building);
            building
        };
        let terminal = build(
            station_centre,
            dir.xy(),
            BuildingKind::RailFreightStation(station_proto),
        );
        let store = build(
            store_centre,
            Vec2::X,
            BuildingKind::GoodsCompany(store_proto),
        );
        let station =
            crate::souls::freight_station::freight_station_soul(&mut sim, terminal, station_proto)
                .unwrap();
        let buyer = company_soul(&mut sim, store, store_proto).unwrap();
        sim.write::<Market>()
            .buy(buyer, store_centre, ItemID::new("goods"), 2);
        let stop = train_stop(&sim.get(station).unwrap().trans);
        sim.apply_commands([&WorldCommand::SpawnTrain {
            wagons: vec![prototypes::RollingStockID::new("locomotive")],
            lane,
            dist: 200.0,
        }]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
        let update = |sim: &mut Simulation, elapsed| {
            *sim.write::<GameTime>() = GameTime::new(Tick(INITIAL_TICK + elapsed));
            let (world, resources) = sim.world_res();
            cargo_update(world, resources);
        };
        update(&mut sim, FREIGHT_MATCH_INTERVAL);
        assert_eq!(
            sim.read::<CargoBook>().routes.rail.get(&terminal),
            Some(&false)
        );
        assert!(sim.read::<CargoBook>().imports.is_empty());
        assert!(matches!(
            sim.read::<CargoBook>().import_status(buyer),
            Some(ImportStatus::NoRoute)
        ));

        let topology = sim.map().walking_topology_generation;
        sim.place_authored_rail_market(RailMarketSite {
            footprint: OBB::new(spawn.xy(), dir.xy(), 100.0, 24.0),
            service_lane: lane,
            service_offset: 200.0,
            platform_stop: stop,
            consist_length_m: 100.0,
        })
        .unwrap();
        assert_eq!(sim.map().walking_topology_generation, topology);
        update(&mut sim, 2 * FREIGHT_MATCH_INTERVAL);
        let book = sim.read::<CargoBook>();
        assert_eq!(book.routes.rail.get(&terminal), Some(&true));
        assert!(matches!(
            book.import_status(buyer),
            Some(ImportStatus::Waiting { .. })
        ));
    }

    /// Long enough for a station truck to drive across the fixture town. Vehicles
    /// move in real time.
    const TRUCK_TRIP: u64 = 6 * 60 * TICKS_PER_REALTIME_SECOND;

    fn station_trucks_parked(f: &Fixture) -> bool {
        let book = f.ctx.g.read::<CargoBook>();
        !book.station_trucks.is_empty()
            && book.station_trucks.iter().all(|(&truck, fleet)| {
                fleet.trip.is_none()
                    && matches!(
                        f.ctx.g.world().vehicles[truck].vehicle.state,
                        VehicleState::ParkedAtDepot
                    )
            })
    }

    #[test]
    fn imports_a_store_waits_for_goods_a_station_truck_brings() {
        let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
        let terminal = import_station(&mut f);
        assert!(!f.ctx.g.world().trains.is_empty());
        let SoulID::FreightStation(station) = terminal.soul else {
            unreachable!()
        };
        let goods = ItemID::new("goods");
        let store = f.destination;
        let hand = |f: &Fixture| f.ctx.g.read::<Market>().capital(store.soul, goods);
        let before = import_spend(&f);

        // The next match orders the goods by rail. Nothing arrives without a train call.
        for _ in 0..=FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
        }
        let wanted = f.ctx.g.get(station).unwrap().f.wanted_cargo;
        let input = fixture_input(&f.ctx.g, store, goods).unwrap();
        assert_eq!(wanted, import_target(input));
        assert!(wanted > input.amount, "a reorder covers the lead time");
        assert!(matches!(
            f.ctx.g.read::<CargoBook>().import_status(store.soul),
            Some(ImportStatus::Waiting { .. })
        ));
        assert_eq!(hand(&f), 0);
        assert_eq!(import_spend(&f), before);

        // A train call puts the order in station stock. A station truck drives it over.
        let left = f
            .ctx
            .g
            .write::<CargoBook>()
            .unload_train(terminal.soul, TRAIN_CAPACITY);
        assert_eq!(left, 0);
        assert_eq!(
            f.ctx.g.read::<CargoBook>().import_stock(terminal.building)[&goods],
            u64::from(wanted)
        );
        let door = f.ctx.g.map().buildings()[terminal.building].door_pos;
        let mut drove = false;
        let mut ticks = 0;
        while hand(&f) == 0 {
            f.ctx.tick_fast();
            ticks += 1;
            assert!(ticks < TRUCK_TRIP, "the station truck never arrived");
            let book = f.ctx.g.read::<CargoBook>();
            drove |= book.station_trucks.keys().any(|&truck| {
                let vehicle = &f.ctx.g.world().vehicles[truck];
                matches!(vehicle.vehicle.state, VehicleState::Driving)
                    && vehicle.trans.pos.distance(door) > 20.0
            });
        }
        assert!(drove, "the goods came by road");
        let delivered = hand(&f) as u32;
        assert_eq!(delivered, wanted.min(STATION_TRUCK_CAPACITY));
        let value = f.ctx.g.read::<Market>().inner()[&goods].ext_value;
        let paid =
            value * (i64::from(delivered) * (100 + crate::economy::IMPORT_FREIGHT_PERCENT)) / 100;
        assert_eq!(import_spend(&f) - before, -i128::from(paid.inner()));
        assert!(
            paid > value * i64::from(delivered),
            "the price includes freight"
        );
        let receipt = f
            .ctx
            .g
            .read::<CargoBook>()
            .freight_day(0)
            .unwrap()
            .receipt(store.soul, goods);
        assert_eq!(receipt.station_units, u64::from(delivered));

        // The rest of the order follows, and the fleet returns to the station.
        for _ in 0..TRUCK_TRIP {
            f.ctx.tick_fast();
            if hand(&f) as u32 >= wanted && station_trucks_parked(&f) {
                break;
            }
        }
        assert!(hand(&f) as u32 >= wanted);
        assert!(station_trucks_parked(&f), "station trucks return");
        assert_eq!(f.ctx.g.read::<CargoBook>().import_status(store.soul), None);
    }

    #[test]
    fn imports_a_store_that_imports_reorders_before_running_out() {
        for above in [false, true] {
            let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
            let terminal = import_station(&mut f);
            let SoulID::FreightStation(station) = terminal.soul else {
                unreachable!()
            };
            let goods = ItemID::new("goods");
            let store = f.destination;
            let input = fixture_input(&f.ctx.g, store, goods).unwrap();
            let reorder = import_reorder_point(input);
            assert!(reorder > input.amount && reorder < import_target(input));
            // An earlier import left the store with stock on hand and no buy order.
            let held = if above { reorder } else { reorder - 1 };
            {
                let (_, resources) = f.ctx.g.world_res();
                let mut book = resources.write::<CargoBook>();
                let delivery = [ItemQuantity {
                    item: goods,
                    units: held,
                }];
                unload_import(&mut book, resources, terminal, store, &delivery, 0);
            }
            assert!(f.ctx.g.read::<Market>().inner()[&goods]
                .buy_order(store.soul)
                .is_none());
            for _ in 0..=FREIGHT_MATCH_INTERVAL {
                f.ctx.tick_fast();
            }
            let wanted = f.ctx.g.get(station).unwrap().f.wanted_cargo;
            if above {
                assert_eq!(wanted, 0, "no reorder above the reorder point");
            } else {
                assert_eq!(wanted, import_target(input) - held);
            }
        }
    }

    #[test]
    fn imports_stop_at_a_removed_station_and_name_the_missing_route() {
        let mut f = Fixture::new();
        let terminal = import_station(&mut f);
        for _ in 0..=FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
        }
        assert!(f
            .ctx
            .g
            .read::<CargoBook>()
            .import_status(f.destination.soul)
            .is_some());
        f.ctx
            .g
            .write::<CargoBook>()
            .unload_train(terminal.soul, TRAIN_CAPACITY);
        f.ctx.tick_fast();
        f.ctx
            .apply(&[WorldCommand::MapRemoveBuilding(terminal.building)]);
        for _ in 0..=2 * FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
        }
        let book = f.ctx.g.read::<CargoBook>();
        assert!(book.imports.is_empty() && book.station_trucks.is_empty());
        assert_eq!(
            book.import_status(f.destination.soul),
            Some(ImportStatus::NoRoute)
        );
        assert_eq!(
            book.import_problems(0).no_route,
            vec![f.destination.building]
        );
        drop(book);
        assert_eq!(f.hand(f.destination), 0);
    }

    #[test]
    fn imports_a_station_truck_that_stops_moving_returns_its_cargo() {
        let mut f = Fixture::with_companies("industrial-l1", "commercial-l1");
        let terminal = import_station(&mut f);
        let goods = ItemID::new("goods");
        for _ in 0..=FREIGHT_MATCH_INTERVAL {
            f.ctx.tick_fast();
        }
        f.ctx
            .g
            .write::<CargoBook>()
            .unload_train(terminal.soul, TRAIN_CAPACITY);
        let door = f.ctx.g.map().buildings()[terminal.building].door_pos;
        let loaded_truck = |f: &Fixture| {
            let book = f.ctx.g.read::<CargoBook>();
            book.station_trucks.iter().find_map(|(&truck, fleet)| {
                let trip = fleet.trip.as_ref()?;
                let vehicle = &f.ctx.g.world().vehicles[truck];
                (!trip.returning
                    && matches!(vehicle.vehicle.state, VehicleState::Driving)
                    && vehicle.trans.pos.distance(door) > 20.0)
                    .then(|| (truck, trip.items.iter().map(|line| line.units).sum::<u32>()))
            })
        };
        let mut on_the_road = None;
        for _ in 0..TRUCK_TRIP {
            f.ctx.tick_fast();
            on_the_road = loaded_truck(&f);
            if on_the_road.is_some() {
                break;
            }
        }
        let (truck, loaded) = on_the_road.expect("a station truck drove off loaded");
        assert!(loaded > 0);
        let stock = |f: &Fixture| {
            f.ctx
                .g
                .read::<CargoBook>()
                .import_stock(terminal.building)
                .get(&goods)
                .copied()
                .unwrap_or(0)
        };
        let before = stock(&f);
        let trip = |f: &Fixture| {
            f.ctx.g.read::<CargoBook>().station_trucks[&truck]
                .trip
                .is_some()
        };

        // The truck stands still from here on, without a routing error.
        let start = elapsed_ticks(&f.ctx.g.resources).0;
        f.update(start + 1);
        f.update(start + LOAD_STALL_TICKS / 2);
        assert!(trip(&f), "a short stop keeps the trip");
        let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&f.ctx.g).unwrap()).unwrap();
        f.update(start + 1 + LOAD_STALL_TICKS);
        update_sim(&mut resumed, start + 1 + LOAD_STALL_TICKS);
        assert_eq!(
            f.ctx.g.hashes(),
            resumed.hashes(),
            "the stall timer is saved"
        );
        assert!(!trip(&f), "a stalled trip is called off");
        assert!(matches!(
            f.ctx.g.world().vehicles[truck].vehicle.state,
            VehicleState::ParkedAtDepot
        ));
        assert_eq!(
            stock(&f),
            before + u64::from(loaded),
            "the cargo is back in station stock"
        );
        assert_eq!(f.hand(f.destination), 0);
    }
}

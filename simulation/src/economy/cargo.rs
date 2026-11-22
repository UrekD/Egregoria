//! Road freight owns stock from reservation through delivery. Market capital is on-hand stock.
//!
//! | Lane | Stock handoff | City budget and rail counter |
//! | --- | --- | --- |
//! | Virtual local (including growables with road freight disabled) | Market match | Neither |
//! | Virtual station import/export | Market match | Import debit + wanted units / export credit + waiting units |
//! | Producer export without a dispatchable truck or road route | Direct export at match, after local recipes keep one truckload | Export credit + waiting units |
//! | Physical company → company | Source debited at reservation, destination credited at verified unload | Neither |
//! | Physical company → station | Source debited at reservation, station receives at verified unload | Export credit + waiting units at unload |
//!
//! Blocked routes release their stock first. A live station can then handle a
//! separate virtual trade. Cancellation never posts an import or export.
//! An outbound leg that keeps failing in the router suspends its route until the
//! cooldown ends or the road network changes, so the stock can take another lane.

use std::collections::{BTreeMap, BTreeSet};

use ordered_float::OrderedFloat;
use prototypes::{
    GoodsCompanyPrototype, ItemID, Money, Tick, WarehouseSpec, TICKS_PER_HOUR, TICKS_PER_MINUTE,
    TICKS_PER_REALTIME_SECOND,
};
use serde::{Deserialize, Serialize};

use super::{
    Budget, BudgetCategory, EcoStats, FreightDay, Market, Trade, TradeTarget, TICKS_PER_BUDGET_DAY,
};
use crate::map::{BuildingID, LaneID, LaneKind, Map, PathKind, RoadID};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{BuildingInfos, Destination, Itinerary, ParkingManagement};
use crate::souls::desire::WorkKind;
use crate::souls::household::household_items;
use crate::transportation::{Location, VehicleKind};
use crate::utils::resources::Resources;
use crate::{CompanyID, HumanEnt, HumanID, ParCommandBuffer, Simulation, SoulID, VehicleID, World};

pub use prototypes::WarehousePolicy;

const FREIGHT_MATCH_INTERVAL: u64 = TICKS_PER_MINUTE / 2;
const DELIVERY_LEAD: u64 = TICKS_PER_HOUR;
// The standard ten-hour driver shift leaves a fourteen-hour replenishment gap.
const BETWEEN_DRIVER_SHIFTS: u64 = 14 * TICKS_PER_HOUR;
const DELIVERY_EXPIRY: u64 = 6 * TICKS_PER_HOUR;
const FREIGHT_PACE_METERS_PER_SECOND: f32 = 8.0;
// Router errors such as a full car park clear on their own. Give up on a leg
// only after it has failed continuously for this long.
const ROUTING_RETRY_WINDOW: u64 = 10 * TICKS_PER_MINUTE;
// A route whose outbound leg gave up is skipped by matching for this long.
const FAILED_ROUTE_COOLDOWN: u64 = 2 * TICKS_PER_HOUR;
const FREIGHT_DAYS_KEPT: u64 = 8;

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
        infos.owner(self.building) == Some(self.soul)
            && map.get(self.building).is_some()
            && match self.soul {
                SoulID::GoodsCompany(id) => world
                    .companies
                    .get(id)
                    .is_some_and(|entity| entity.comp.building == self.building),
                SoulID::FreightStation(id) => world
                    .freight_stations
                    .get(id)
                    .is_some_and(|entity| entity.f.building == self.building),
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
    topology: u64,
    roads: [Option<RoadID>; 2],
}

#[derive(Default, Serialize, Deserialize)]
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
    #[serde(skip)]
    routes: RouteCache,
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
        if book.policies.insert(company, policy) == Some(policy) {
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
        let mut units = BTreeMap::new();
        for load in self.loads.values() {
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
        let suspended = self.suspended.get(&(from, to)).is_some_and(|route| {
            now < route.until
                && route.topology == map.walking_topology_generation
                && route.roads == connected_roads(map, from, to)
        });
        !suspended && self.routes.reaches(map, from.building, to.building)
    }

    fn suspend_route(&mut self, map: &Map, from: Endpoint, to: Endpoint, now: u64) {
        self.suspended.insert(
            (from, to),
            SuspendedRoute {
                until: now + FAILED_ROUTE_COOLDOWN,
                topology: map.walking_topology_generation,
                roads: connected_roads(map, from, to),
            },
        );
    }
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
    component: LaneID,
    drive_pos: geom::Vec3,
}

/// Road reachability for freight, derived from the map and rebuilt when the
/// lane topology changes. Reachability compares strongly connected driving
/// components, so it never runs a route search. Driven lengths, which only
/// warehouses compare, take one search per ordered pair.
#[derive(Default)]
struct RouteCache {
    revision: (u64, u64),
    components: Option<BTreeMap<LaneID, LaneID>>,
    // Keyed by the building's road connection, which can change without a
    // lane topology change.
    access: BTreeMap<BuildingID, (Option<RoadID>, Option<ParkingAccess>)>,
    distances: BTreeMap<(BuildingID, BuildingID), Option<OrderedFloat<f32>>>,
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
        let components = self
            .components
            .get_or_insert_with(|| driving_components(map));
        let access = (|| {
            let connected = map.get(road?)?;
            let spot = ParkingManagement::default()
                .reserve_near(building.door_pos, map)
                .ok()?;
            let drive_pos = spot.park_pos(map)?;
            let lane = map.nearest_lane(drive_pos, LaneKind::Driving, None)?;
            let component = *components.get(&lane)?;
            connected
                .lanes_iter()
                .any(|(lane, _)| components.get(&lane) == Some(&component))
                .then_some(ParkingAccess {
                    component,
                    drive_pos,
                })
        })();
        self.access.insert(id, (road, access));
        access
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

/// Labels each driving or bus lane with the smallest lane of its strongly
/// connected component. Iterative Kosaraju, in lane order for determinism.
fn driving_components(map: &Map) -> BTreeMap<LaneID, LaneID> {
    let drivable = |kind| matches!(kind, LaneKind::Driving | LaneKind::Bus);
    let lanes: Vec<LaneID> = map
        .lanes()
        .iter()
        .filter(|(_, lane)| drivable(lane.kind))
        .map(|(id, _)| id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let index: BTreeMap<_, _> = lanes.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let mut forward = vec![Vec::new(); lanes.len()];
    let mut reverse = vec![Vec::new(); lanes.len()];
    for (src, &lane) in lanes.iter().enumerate() {
        let Some(intersection) = map.intersections().get(map.lanes()[lane].dst) else {
            continue;
        };
        for (turn, _) in intersection.turns_from(lane) {
            if let Some(&dst) = index.get(&turn.dst) {
                forward[src].push(dst);
                reverse[dst].push(src);
            }
        }
    }

    let mut visited = vec![false; lanes.len()];
    let mut finished = Vec::with_capacity(lanes.len());
    for root in 0..lanes.len() {
        if visited[root] {
            continue;
        }
        visited[root] = true;
        let mut stack = vec![(root, 0)];
        while let Some(top) = stack.last_mut() {
            let node = top.0;
            if let Some(&next) = forward[node].get(top.1) {
                top.1 += 1;
                if !visited[next] {
                    visited[next] = true;
                    stack.push((next, 0));
                }
            } else {
                stack.pop();
                finished.push(node);
            }
        }
    }

    let mut label = vec![usize::MAX; lanes.len()];
    for &root in finished.iter().rev() {
        if label[root] != usize::MAX {
            continue;
        }
        let mut members = vec![root];
        label[root] = root;
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            for &prev in &reverse[node] {
                if label[prev] == usize::MAX {
                    label[prev] = root;
                    members.push(prev);
                    pending.push(prev);
                }
            }
        }
        let smallest = *members.iter().min().unwrap();
        for member in members {
            label[member] = smallest;
        }
    }
    lanes
        .iter()
        .enumerate()
        .map(|(i, &lane)| (lane, lanes[label[i]]))
        .collect()
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
    fn new(destination: Endpoint, amount: i32, recipe: &prototypes::Recipe) -> Self {
        Self {
            destination,
            amount: amount as u32,
            recipe_ticks: (recipe.duration.seconds() as f64 / f64::from(prototypes::DELTA)).ceil()
                as u64,
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
    // A warehouse runs one driver per truck from its staff. Producers have one driver.
    let drivers = if entity.comp.proto.prototype().storage.is_some() {
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
        (entity.comp.driver == Some(driver) || entity.comp.proto.prototype().storage.is_some())
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

struct Matching<'a> {
    book: &'a mut CargoBook,
    market: &'a mut Market,
    map: &'a Map,
    queued: BTreeMap<StockKey, u32>,
    in_transit: BTreeMap<StockKey, u32>,
    outbound: BTreeSet<StockKey>,
    now: u64,
}

impl Matching<'_> {
    fn queue(&mut self, source: Endpoint, destination: Endpoint, quantity: ItemQuantity) {
        self.book.reserve(
            self.market,
            Shipment {
                source,
                destination,
                quantity,
                reserved_at: self.now,
            },
        );
        *self.queued.entry((destination, quantity.item)).or_default() += quantity.units;
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

    fn reserve_for_input(
        &mut self,
        policy: DispatchPolicy,
        source: Endpoint,
        input: Input,
        item: ItemID,
        available: u32,
    ) -> u32 {
        let key = (input.destination, item);
        let pending = self.queued.get(&key).copied().unwrap_or(0);
        let stock = on_hand(self.market, input.destination, item);
        let deficit = self.deficit(policy, input, item);
        // Allow smaller orders when some stock is queued or the recipe cannot run.
        if pending == 0 && deficit < policy.minimum && stock >= input.amount {
            return 0;
        }
        let units = deficit.min(available);
        if units > 0 {
            self.queue(source, input.destination, ItemQuantity { item, units });
        }
        units
    }

    fn route(&mut self, from: Endpoint, to: Endpoint) -> bool {
        self.book.physical_route(self.map, from, to, self.now)
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
        let mut used_locally = BTreeSet::new();
        for (&item, available) in &mut stock {
            let mut candidates = inputs.get(&item).cloned().unwrap_or_default();
            candidates.sort_unstable_by_key(|input| {
                (
                    on_hand(self.market, input.destination, item) >= input.amount,
                    straight_distance(self.map, source, input.destination),
                )
            });
            for input in candidates {
                let Some(distance) = self.road_distance(source, input.destination) else {
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
                *available -= self.reserve_for_input(policy, source, input, item, *available);
            }
        }

        // The watermarks apply to the shared space. Items no reachable recipe
        // uses are surplus down to the low watermark and leave first. Items
        // local recipes use leave only from above the high watermark.
        if warehouse.policy != WarehousePolicy::ReserveLocally {
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
                    self.queue(source, station, ItemQuantity { item, units });
                    *available -= units;
                    above_low -= units;
                    above_high = above_high.saturating_sub(units);
                }
            }
        }

        for (item, units) in before {
            let sent = units - stock[&item];
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
            let units = available
                .min(limit.saturating_sub(warehouse.staged))
                .min(room);
            if units == 0 {
                continue;
            }
            self.queue(source, dest, ItemQuantity { item, units });
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
        if !destination.live(world, map, infos) {
            continue;
        }
        for item in recipe
            .consumption
            .iter()
            .filter(|item| items.contains(&item.id))
        {
            inputs
                .entry(item.id)
                .or_default()
                .push(Input::new(destination, item.amount, recipe));
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
    let mut market = resources.write::<Market>();
    let mut matching = Matching {
        queued: book.queued(),
        in_transit: book.in_transit(),
        book,
        market: &mut market,
        map,
        outbound: BTreeSet::new(),
        now,
    };
    let mut warehouses = Vec::with_capacity(warehouse_sites.len());
    if !warehouse_sites.is_empty() {
        let mut staffed_producers = BTreeMap::<_, Vec<_>>::new();
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
            let mut warehouse = Warehouse {
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
            };
            if !released_sources.contains(&endpoint) {
                matching.warehouse_output(&mut warehouse, &inputs, &staffed_producers, &stations);
            }
            // A zero low watermark restocks once the warehouse is empty.
            if warehouse.staged < spec.low_water_units.max(1) {
                matching.book.restocking.insert(id);
            } else if warehouse.staged >= spec.high_water_units {
                matching.book.restocking.remove(&id);
            }
            warehouses.push(warehouse);
        }
    }

    for (id, source, policy, recipe) in sources {
        let has_driver = assigned_driver(world, source, id).is_some();
        let has_fleet = has_fleet(world, id);
        let distance = |dest| straight_distance(map, source, dest);
        for output in &recipe.production {
            let item = output.id;
            if household_items().contains(&item) {
                continue;
            }
            // Drop any sell order carried into this version from the old instantaneous lane.
            matching.market.hide_freight_stock(source.soul, item);
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
            let mut local = false;
            for input in candidates {
                if !matching.route(source, input.destination) {
                    continue;
                }
                local = true;
                if !has_driver {
                    break;
                }
                available -= matching.reserve_for_input(policy, source, input, item, available);
                if available == 0 {
                    break;
                }
            }

            // Keep one truckload on hand for the next local reorder. A firm
            // without a truck cannot deliver it, so its stock may leave by rail.
            if local && has_fleet {
                available = available.saturating_sub(policy.capacity);
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

            let physical_station = (has_driver && !world.companies[id].comp.proto.is_growable())
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
                        s.source == source && s.destination == station && s.quantity.item == item
                    })
                    .map(|s| s.quantity.units)
                    .sum();
                let units = available.min(policy.capacity.saturating_sub(pending));
                if units != 0 {
                    matching.queue(source, station, ItemQuantity { item, units });
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
                destination_input(world, destination, item)
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
                    let Some(input) = destination_input(world, destination, item) else {
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
            let nominal_travel = (from.distance(to) / FREIGHT_PACE_METERS_PER_SECOND
                * TICKS_PER_REALTIME_SECOND as f32)
                .ceil() as u64;
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
            let human = &mut world.humans[driver];
            let work = human.work.as_mut().unwrap();
            work.kind = WorkKind::Driver {
                truck,
                deliver_order: Some(destination.building),
                load: Some(load),
            };
            // Do not replace an existing decision. The work scorer picks up this
            // assignment while the driver is at the depot.
            remaining.retain(|id| book.shipments.contains_key(id));
        }
    }
}

fn destination_input(world: &World, destination: Endpoint, item: ItemID) -> Option<Input> {
    let SoulID::GoodsCompany(id) = destination.soul else {
        return None;
    };
    let recipe = world
        .companies
        .get(id)?
        .comp
        .proto
        .prototype()
        .recipe
        .as_ref()?;
    let input = recipe.consumption.iter().find(|input| input.id == item)?;
    Some(Input::new(destination, input.amount, recipe))
}

/// Records the station side of a virtual match. The ordinary market still
/// owns its budget, stock and trade history.
pub fn settle_physical_match(world: &mut World, resources: &Resources, trade: Trade) -> bool {
    if matches!(
        trade.buyer.0,
        SoulID::GoodsCompany(_) | SoulID::FreightStation(_)
    ) {
        record_rail_handoff(world, trade);
    }
    // A station import fills a recipe that local freight left short.
    if let (SoulID::GoodsCompany(_), SoulID::FreightStation(_)) = (trade.buyer.0, trade.seller.0) {
        let now = elapsed_ticks(resources).0;
        resources
            .write::<CargoBook>()
            .day_mut(now)
            .receipt_mut(trade.buyer.0, trade.kind)
            .station_units += trade.qty.max(0) as u64;
    }
    false
}

fn record_rail_handoff(world: &mut World, trade: Trade) {
    if let SoulID::FreightStation(id) = trade.buyer.0 {
        if let Some(station) = world.freight_stations.get_mut(id) {
            station.f.accept_export(trade.qty as u32);
        }
    }
    if let SoulID::FreightStation(id) = trade.seller.0 {
        if let Some(station) = world.freight_stations.get_mut(id) {
            station.f.request_import(trade.qty as u32);
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
                        entity.comp.proto.prototype().storage.is_none()
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
    release(
        &mut book,
        world,
        resources,
        shipments,
        loads,
        Release::Cancelled,
    );
}

fn clear_assignment(world: &mut World, manifest: &TruckLoad, token: LoadId) {
    let Some(work) = world
        .humans
        .get_mut(manifest.driver)
        .and_then(|human| human.work.as_mut())
    else {
        return;
    };
    if let WorkKind::Driver {
        load,
        deliver_order,
        ..
    } = &mut work.kind
    {
        if *load == Some(token) {
            *load = None;
            *deliver_order = None;
        }
    }
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

/// A route error, or a failed reroute of the driver or of the truck they ride.
fn routing_failed(human: &HumanEnt, vehicle: &crate::VehicleEnt, truck: VehicleID) -> bool {
    let rerouting = |it: &Itinerary| it.is_wait_for_reroute().is_some_and(|wait| wait > 0);
    human.router.last_error.is_some()
        || rerouting(&human.it)
        || (human.location == Location::Vehicle(truck) && rerouting(&vehicle.it))
}

/// Follows empty trucks home. A return leg that keeps failing, or outlasts a
/// delivery window, gives the truck back to its depot and frees the driver.
fn track_returns(book: &mut CargoBook, world: &World, resources: &Resources, now: u64) {
    let mut stranded = Vec::new();
    book.returning.retain(|&truck, trip| {
        let (Some(human), Some(vehicle)) =
            (world.humans.get(trip.driver), world.vehicles.get(truck))
        else {
            return false;
        };
        let still_assigned = human.work.as_ref().is_some_and(|work| {
            work.workplace == trip.depot.building
                && matches!(work.kind, WorkKind::Driver { truck: assigned, load: None, .. }
                    if assigned == truck)
        });
        let home = human.location == Location::Building(trip.depot.building)
            && vehicle.vehicle.state.is_parked();
        if !still_assigned || home {
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
    if !match_due && book.loads.is_empty() && book.shipments.is_empty() && book.returning.is_empty()
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
    let check_queued =
        match_due || book.routes.revision != (map.identity(), map.walking_topology_generation);
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
            (now >= load.departed_at + DELIVERY_EXPIRY
                || !valid(&mut book.routes, load.source, load.destination)
                || !assigned
                || !holds_load)
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
    track_returns(book, world, resources, now);
    if match_due {
        book.suspended.retain(|_, route| now < route.until);
        match_shipments(book, world, resources, &map, &infos, now, &released_sources);
        if !book.shipments.is_empty() {
            dispatch(book, world, resources, &map, now, &released_sources);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Self::with_freight(source, destination, false)
        }

        fn with_freight(source: &str, destination: &str, growable_freight: bool) -> Self {
            let mut ctx = TestCtx::new_fast();
            if growable_freight {
                crate::souls::goods_company::enable_growable_freight(&mut ctx.g);
            }
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
            let input = destination_input(f.ctx.g.world(), f.destination, Fixture::coal()).unwrap();
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
                assert_eq!(f.hand(depot) as u32 + incoming, spec.high_water_units);
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
        let input = destination_input(f.ctx.g.world(), f.destination, Fixture::coal()).unwrap();
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
        fixture.seed(25, 1);
        fixture.update(FREIGHT_MATCH_INTERVAL);
        let load = fixture.token();
        let (driver, truck, destination) =
            (fixture.driver, fixture.truck, fixture.destination.building);
        assert_eq!(fixture.manifest_units(), 16);
        assert_eq!(fixture.reserved(), 9);
        assert_eq!(fixture.hand(fixture.source), 0);
        assert_eq!(fixture.hand(fixture.destination), 1);
        assert_eq!(
            fixture.ctx.g.read::<CargoBook>().stock_for(destination)[&Fixture::coal()].incoming,
            25
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
        assert_eq!(fixture.hand(fixture.destination), 17);
        assert_eq!(fixture.manifest_units(), 0);
        assert_eq!(fixture.reserved(), 9);
        assert_eq!(fixture.ctx.g.hashes(), resumed.hashes());
        assert!(complete_delivery(&mut fixture.ctx.g, load, driver, truck, destination).is_err());
        let metrics = fixture.ctx.g.read::<DailyEconomyMetrics>();
        assert_eq!(metrics.days[&0].loads, Some(1));
        assert_eq!(metrics.days[&0].load_units, Some(16));
        assert_eq!(metrics.days[&0].late_loads, Some(0));
    }

    #[test]
    fn e2a_destination_reorder_and_aged_partial_dispatch_are_bounded() {
        let mut f = Fixture::new();
        let input = destination_input(f.ctx.g.world(), f.destination, Fixture::coal()).unwrap();
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
    fn e2a_startup_imports_when_mine_has_no_truck_or_road_route() {
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
            assert_eq!(f.hand(f.destination), 1);
            let station = &f.ctx.g.get(station).unwrap().f;
            assert_eq!(station.wanted_cargo, 1);
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

    fn mine_policy() -> DispatchPolicy {
        DispatchPolicy::for_prototype(GoodsCompanyID::new("coal-mine").prototype()).unwrap()
    }

    fn plant_target(f: &Fixture) -> i32 {
        destination_input(f.ctx.g.world(), f.destination, Fixture::coal())
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
    fn e2fix_outbound_router_errors_retry_before_cancelling_then_use_the_station() {
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

        // The failing route is not reserved again. The batch uses the station.
        set_router(&mut f, truck, destination, None);
        f.update(FREIGHT_MATCH_INTERVAL + 31 * TICKS_PER_MINUTE);
        assert_eq!(
            (f.manifest_units(), f.reserved(), f.hand(f.source)),
            (0, 0, 0)
        );
        assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 16);
    }

    #[test]
    fn e2fix_demolition_writes_off_on_hand_queued_and_loaded_freight_once() {
        for destination_first in [true, false] {
            let mut f = Fixture::new();
            f.seed(100, 1);
            f.update(FREIGHT_MATCH_INTERVAL);
            assert_eq!(f.manifest_units(), 16);
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
            assert_eq!(losses, Some(101), "destination first: {destination_first}");
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
        let amount = destination_input(f.ctx.g.world(), f.destination, Fixture::coal())
            .unwrap()
            .amount;
        f.seed(0, amount as i32);
        f.update(FREIGHT_MATCH_INTERVAL);
        assert!(incoming(&f, f.destination.building, Fixture::coal()) > 0);
    }

    #[test]
    fn e2fix_only_staffed_depots_take_producer_stock() {
        let mut f = Fixture::new();
        let idle = warehouse(&mut f, "small-depot");
        let staffed = place_warehouse(
            &mut f,
            GoodsCompanyID::new("small-depot").prototype(),
            vec2(150.0, 450.0),
        );
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
        f.update(FREIGHT_MATCH_INTERVAL);
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
        f.seed(40, target);
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
    fn e2fix_reachability_runs_no_route_search_and_tracks_topology() {
        let mut f = Fixture::new();
        f.seed(100, 1);
        let target = plant_target(&f);
        for step in 1..=4 {
            f.update(step * FREIGHT_MATCH_INTERVAL);
        }
        assert!(f.reserved() > 0 && f.reserved() < target as u32);
        assert_eq!(f.ctx.g.read::<CargoBook>().routes.searches, 0);

        // A road edit relabels the components on the next check.
        let road = f.ctx.g.map().buildings()[f.destination.building]
            .connected_road
            .unwrap();
        f.ctx.apply(&[WorldCommand::MapRemoveRoad(road)]);
        f.update(5 * FREIGHT_MATCH_INTERVAL + 1);
        assert_eq!((f.manifest_units(), f.reserved()), (0, 0));
        assert_eq!(f.ctx.g.read::<CargoBook>().routes.searches, 0);
    }

    #[test]
    fn e2fix_receipts_separate_truck_unloads_from_station_imports() {
        let mut f = Fixture::new();
        f.seed(16, 0);
        f.update(FREIGHT_MATCH_INTERVAL);
        let (driver, truck, at) = (f.driver, f.truck, f.destination.building);
        unload_fixture(&mut f, driver, truck, at);
        let terminal = station(&mut f, false);
        let import = Trade {
            buyer: TradeTarget(f.destination.soul),
            seller: TradeTarget(terminal.soul),
            kind: Fixture::coal(),
            qty: 3,
            money_delta: Money::new_bucks(-3),
        };
        let (world, resources) = f.ctx.g.world_res();
        assert!(!settle_physical_match(world, resources, import));
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
}

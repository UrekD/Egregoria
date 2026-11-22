//! Road freight owns stock from reservation through delivery. Market capital is on-hand stock.
//!
//! | Lane | Stock handoff | City budget and rail counter |
//! | --- | --- | --- |
//! | Virtual local (including growables with road freight disabled) | Market match | Neither |
//! | Virtual station import/export | Market match | Import debit + wanted units / export credit + waiting units |
//! | Physical company → company | Source debited at reservation, destination credited at verified unload | Neither |
//! | Physical company → station | Source debited at reservation, station receives at verified unload | Export credit + waiting units at unload |
//!
//! Blocked routes release their stock first. A live station can then handle a
//! separate virtual trade. Cancellation never posts an import or export.

use std::collections::{BTreeMap, BTreeSet};

use ordered_float::OrderedFloat;
use prototypes::{
    GoodsCompanyPrototype, ItemID, Money, Tick, WarehouseSpec, TICKS_PER_HOUR, TICKS_PER_MINUTE,
    TICKS_PER_REALTIME_SECOND,
};
use serde::{Deserialize, Serialize};

use super::{Budget, BudgetCategory, EcoStats, Market, Trade, TradeTarget, TICKS_PER_BUDGET_DAY};
use crate::map::{BuildingID, LaneKind, Map, PathKind};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{BuildingInfos, Destination, Itinerary};
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
}

impl TruckLoad {
    pub fn source(&self) -> BuildingID {
        self.source.building
    }

    pub fn destination(&self) -> BuildingID {
        self.destination.building
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct CargoBook {
    next_id: u64,
    shipments: BTreeMap<ShipmentId, Shipment>,
    loads: BTreeMap<LoadId, TruckLoad>,
    policies: BTreeMap<CompanyID, WarehousePolicy>,
    restocking: BTreeSet<CompanyID>,
    pub(crate) growable_freight: bool,
    #[serde(skip)]
    routes: RouteCache,
}

#[derive(Debug, Default)]
pub struct Stock {
    pub reserved: u32,
    pub incoming: u32,
}

type StockKey = (Endpoint, ItemID);

impl CargoBook {
    fn next_token(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn manifest_for(&self, vehicle: VehicleID) -> Option<&TruckLoad> {
        self.loads.values().find(|load| load.vehicle == vehicle)
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

    pub(crate) fn set_warehouse_policy(
        sim: &mut Simulation,
        company: CompanyID,
        policy: WarehousePolicy,
    ) {
        let (world, resources) = sim.world_res();
        let mut book = resources.write::<CargoBook>();
        book.policies.insert(company, policy);
        if policy != WarehousePolicy::ReserveLocally {
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
            .filter_map(|(&id, load)| exports(load.source, load.destination).then_some(id))
            .collect();
        release(&mut book, world, resources, shipments, loads);
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

#[derive(Default)]
struct RouteCache {
    revision: (u64, u64),
    by_building: BTreeMap<(BuildingID, BuildingID), Option<[OrderedFloat<f32>; 2]>>,
}

impl RouteCache {
    fn reaches(&mut self, map: &Map, from: BuildingID, to: BuildingID) -> bool {
        self.distance(map, from, to).is_some()
    }

    fn distance(
        &mut self,
        map: &Map,
        from: BuildingID,
        to: BuildingID,
    ) -> Option<OrderedFloat<f32>> {
        let revision = (map.identity(), map.walking_topology_generation);
        if self.revision != revision {
            self.revision = revision;
            self.by_building.clear();
        }
        let key = (from.min(to), from.max(to));
        let (a, b) = map.get(key.0).zip(map.get(key.1))?;
        // Building connections can change independently of the lane graph.
        if ![a, b].into_iter().all(|building| {
            building
                .connected_road
                .and_then(|road| map.get(road))
                .is_some_and(|road| road.lanes_iter().any(|(_, kind)| kind == LaneKind::Driving))
        }) {
            return None;
        }
        let distance = self.by_building.entry(key).or_insert_with(|| {
            Some([
                vehicle_distance(map, a.door_pos, b.door_pos)?,
                vehicle_distance(map, b.door_pos, a.door_pos)?,
            ])
        });
        distance.map(|distance| distance[usize::from(from != key.0)])
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
    let warehouse = entity.comp.proto.prototype().storage.is_some();
    entity
        .comp
        .driver
        .iter()
        .filter(move |_| !warehouse)
        .chain(entity.workers.0.iter().filter(move |_| warehouse))
        .filter_map(move |&driver| Some((driver, driver_truck(world, source, company, driver)?)))
}

fn assigned_driver(
    world: &World,
    source: Endpoint,
    company: CompanyID,
) -> Option<(HumanID, VehicleID)> {
    assigned_drivers(world, source, company).next()
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

struct Warehouse {
    id: CompanyID,
    endpoint: Endpoint,
    spec: &'static WarehouseSpec,
    policy: WarehousePolicy,
    on_hand: u32,
    staged: u32,
    committed: u32,
}

fn route_distance(map: &Map, from: Endpoint, to: Endpoint) -> (OrderedFloat<f32>, Endpoint) {
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
        let incoming = pending + self.in_transit.get(&key).copied().unwrap_or(0);
        let deficit = input
            .target(policy)
            .saturating_sub(stock.saturating_add(incoming));
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

    fn warehouse_output(
        &mut self,
        world: &World,
        warehouse: &mut Warehouse,
        inputs: &BTreeMap<ItemID, Vec<Input>>,
        staffed_producers: &BTreeMap<ItemID, Vec<Endpoint>>,
        stations: &[Endpoint],
    ) {
        let source = warehouse.endpoint;
        let Some(policy) =
            DispatchPolicy::for_prototype(world.companies[warehouse.id].comp.proto.prototype())
        else {
            return;
        };
        if assigned_driver(world, source, warehouse.id).is_none() {
            return;
        }
        for &item in &warehouse.spec.accepted_items {
            let stock = on_hand(self.market, source, item);
            let mut available = stock;
            if available == 0 {
                continue;
            }
            let mut candidates = inputs.get(&item).cloned().unwrap_or_default();
            candidates.sort_unstable_by_key(|input| {
                (
                    on_hand(self.market, input.destination, item) >= input.amount,
                    route_distance(self.map, source, input.destination),
                )
            });
            let mut local = false;
            for input in candidates {
                let dest = input.destination;
                let Some(warehouse_distance) = self.road_distance(source, dest) else {
                    continue;
                };
                local = true;
                let short = on_hand(self.market, dest, item) < input.amount;
                let faster_direct = !short
                    && staffed_producers
                        .get(&item)
                        .into_iter()
                        .flatten()
                        .any(|&producer| {
                            self.road_distance(producer, dest)
                                .is_some_and(|distance| distance <= warehouse_distance)
                        });
                if faster_direct {
                    continue;
                }
                available -= self.reserve_for_input(policy, source, input, item, available);
                if available == 0 {
                    break;
                }
            }

            // Balanced can release inventory once its local route disappears. Explicit
            // surplus staging keeps a full local buffer while forwarding excess.
            let export_floor = match warehouse.policy {
                WarehousePolicy::ReserveLocally => None,
                WarehousePolicy::Balanced if !local => Some(warehouse.spec.low_water_units),
                _ => Some(warehouse.spec.high_water_units),
            };
            if let Some(floor) =
                export_floor.filter(|_| available > 0 && !item.prototype().optout_exttrade)
            {
                let units = available
                    .min(
                        warehouse
                            .on_hand
                            .saturating_sub(stock - available)
                            .saturating_sub(floor),
                    )
                    .min(policy.capacity);
                if let Some(station) = (units > 0)
                    .then(|| self.nearest_station(source, stations))
                    .flatten()
                {
                    // Keep at most one unsent export batch per item.
                    if !self.book.shipments.values().any(|s| {
                        s.source == source && s.destination == station && s.quantity.item == item
                    }) {
                        self.queue(source, station, ItemQuantity { item, units });
                        available -= units;
                    }
                }
            }
            if available < stock {
                warehouse.staged -= stock - available;
                warehouse.on_hand -= stock - available;
                self.outbound.insert((source, item));
            }
        }
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

    fn warehouse_input(
        &mut self,
        source: Endpoint,
        item: ItemID,
        inputs: &[Input],
        warehouses: &mut [Warehouse],
        stations: &[Endpoint],
        mut available: u32,
    ) -> u32 {
        let mut candidates: Vec<_> = warehouses
            .iter_mut()
            .filter(|warehouse| {
                warehouse.spec.accepted_items.contains(&item)
                    && !self.outbound.contains(&(warehouse.endpoint, item))
            })
            .collect();
        candidates
            .sort_unstable_by_key(|warehouse| route_distance(self.map, source, warehouse.endpoint));
        for warehouse in candidates {
            let dest = warehouse.endpoint;
            let room = warehouse
                .spec
                .capacity_units
                .saturating_sub(warehouse.committed);
            if room == 0 {
                continue;
            }
            let replenishing = self.book.restocking.contains(&warehouse.id)
                && inputs.iter().any(|input| {
                    let consumer = input.destination;
                    self.road_distance(dest, consumer).is_some_and(|local| {
                        on_hand(self.market, consumer, item) < input.amount
                            || self
                                .road_distance(source, consumer)
                                .is_none_or(|direct| local < direct)
                    })
                });
            let surplus = warehouse.policy == WarehousePolicy::ExportSurplus
                && !item.prototype().optout_exttrade
                && warehouse.staged >= warehouse.spec.high_water_units
                && self.nearest_station(dest, stations).is_some_and(|station| {
                    let direct = self
                        .road_distance(source, station)
                        .unwrap_or(OrderedFloat(f32::INFINITY));
                    self.road_distance(dest, station)
                        .is_some_and(|local| local < direct)
                        && self
                            .road_distance(source, dest)
                            .is_some_and(|inbound| inbound < direct)
                });
            let target = if surplus {
                warehouse.spec.capacity_units
            } else if replenishing {
                warehouse.spec.high_water_units
            } else {
                continue;
            };
            let units = available
                .min(target.saturating_sub(warehouse.staged))
                .min(room);
            if units == 0 || self.road_distance(source, dest).is_none() {
                continue;
            }
            self.queue(source, dest, ItemQuantity { item, units });
            warehouse.staged += units;
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

    fn road_distance(&mut self, from: Endpoint, to: Endpoint) -> Option<OrderedFloat<f32>> {
        if from == to {
            return None;
        }
        self.book
            .routes
            .distance(self.map, from.building, to.building)
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
            let spec = company.comp.proto.prototype().storage.as_ref()?;
            let endpoint = Endpoint {
                soul: SoulID::GoodsCompany(id),
                building: company.comp.building,
            };
            endpoint
                .live(world, map, infos)
                .then_some((id, endpoint, spec))
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
                .flat_map(|(_, _, spec)| spec.accepted_items.iter().copied()),
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
        let mut returnable = BTreeMap::<_, u32>::new();
        for shipment in matching.book.shipments.values() {
            *returnable.entry(shipment.source).or_default() += shipment.quantity.units;
        }
        for load in matching.book.loads.values() {
            *returnable.entry(load.source).or_default() +=
                load.items.iter().map(|line| line.units).sum::<u32>();
        }
        for &(id, source, _, recipe) in &sources {
            if assigned_driver(world, source, id).is_some() {
                for item in &recipe.production {
                    staffed_producers.entry(item.id).or_default().push(source);
                }
            }
        }
        for (id, endpoint, spec) in warehouse_sites {
            let on_hand = spec
                .accepted_items
                .iter()
                .map(|&item| on_hand(matching.market, endpoint, item))
                .sum::<u32>();
            let incoming = spec
                .accepted_items
                .iter()
                .map(|&item| {
                    let key = (endpoint, item);
                    matching.queued.get(&key).copied().unwrap_or(0)
                        + matching.in_transit.get(&key).copied().unwrap_or(0)
                })
                .sum::<u32>();
            let mut warehouse = Warehouse {
                id,
                endpoint,
                spec,
                policy: matching.book.warehouse_policy(id, spec),
                on_hand,
                staged: on_hand + incoming,
                // Outbound stock can still be returned after a cancellation.
                committed: on_hand + incoming + returnable.get(&endpoint).copied().unwrap_or(0),
            };
            if !released_sources.contains(&endpoint) {
                matching.warehouse_output(
                    world,
                    &mut warehouse,
                    &inputs,
                    &staffed_producers,
                    &stations,
                );
            }
            if warehouse.staged < spec.low_water_units {
                matching.book.restocking.insert(id);
            } else if warehouse.staged >= spec.high_water_units {
                matching.book.restocking.remove(&id);
            }
            warehouses.push(warehouse);
        }
    }

    for (id, source, policy, recipe) in sources {
        let has_driver = assigned_driver(world, source, id).is_some();
        let distance = |dest| route_distance(map, source, dest);
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
            let mut local = false;
            if has_driver {
                let mut candidates = inputs.get(&item).cloned().unwrap_or_default();
                candidates.sort_unstable_by_key(|input| distance(input.destination));
                for input in candidates {
                    let dest = input.destination;
                    if source == dest
                        || !matching
                            .book
                            .routes
                            .reaches(map, source.building, dest.building)
                    {
                        continue;
                    }
                    local = true;
                    available -= matching.reserve_for_input(policy, source, input, item, available);
                    if available == 0 {
                        break;
                    }
                }
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

            // Give reachable local recipes first claim. Keep stock at the depot
            // for their next reorder before offering surplus to a station.
            let available = available.saturating_sub(if local { policy.capacity } else { 0 });
            let Some(station) = stations
                .iter()
                .copied()
                .min_by_key(|&station| distance(station))
                .filter(|_| available > 0)
            else {
                continue;
            };
            if item.prototype().optout_exttrade {
                continue;
            }
            if has_driver
                && !world.companies[id].comp.proto.is_growable()
                && matching
                    .book
                    .routes
                    .reaches(map, source.building, station.building)
            {
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
            } else if available >= policy.minimum {
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
    // Warehouses may dispatch more than one truck. An earlier ineligible route does not
    // hold a later ready route, and shipment IDs break all FIFO ties.
    let mut routes = BTreeMap::<_, Vec<_>>::new();
    for (&id, shipment) in &book.shipments {
        routes
            .entry((shipment.source, shipment.destination))
            .or_default()
            .push(id);
    }
    let mut routes: Vec<_> = routes.into_iter().collect();
    routes.sort_unstable_by_key(|(_, ids)| ids[0]);
    let market = resources.read::<Market>();
    let time = resources.read::<prototypes::GameTime>();
    let mut in_transit = book.in_transit();
    for ((source, destination), ids) in routes {
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
    let recipe = world.companies[id].comp.proto.prototype().recipe.as_ref()?;
    let input = recipe.consumption.iter().find(|input| input.id == item)?;
    Some(Input::new(destination, input.amount, recipe))
}

/// Records the station side of a virtual match. The ordinary market still
/// owns its budget, stock and trade history.
pub fn settle_physical_match(world: &mut World, _resources: &Resources, trade: Trade) -> bool {
    if matches!(
        trade.buyer.0,
        SoulID::GoodsCompany(_) | SoulID::FreightStation(_)
    ) {
        record_rail_handoff(world, trade);
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

/// Unload after the assigned driver rode the assigned truck to the live recipient.
pub fn complete_delivery(
    sim: &mut Simulation,
    load: LoadId,
    driver: HumanID,
    truck: VehicleID,
    at: BuildingID,
) -> bool {
    let (world, resources) = sim.world_res();
    let mut book = resources.write::<CargoBook>();
    let Some(manifest) = book.loads.get(&load) else {
        return false;
    };
    let now = elapsed_ticks(resources).0;
    if manifest.driver != driver || manifest.vehicle != truck || manifest.destination() != at
        || !manifest.used_truck || now >= manifest.departed_at + DELIVERY_EXPIRY
        || !manifest.destination.live(world, &resources.read::<Map>(), &resources.read::<BuildingInfos>())
        || !manifest.source.live(world, &resources.read::<Map>(), &resources.read::<BuildingInfos>())
        || !world.humans.get(driver).is_some_and(|human| {
            human.location == Location::Building(at)
                && human.router.selected_vehicle() == Some(truck)
                && human.work.as_ref().is_some_and(|work| {
                    work.workplace == manifest.source.building && matches!(work.kind,
                        WorkKind::Driver { truck: assigned, load: Some(token), deliver_order: Some(dest) }
                        if assigned == truck && token == load && dest == at)
                })
        })
        || !world.vehicles.get(truck).is_some_and(|entity| entity.vehicle.state.is_parked())
        || !matches!(manifest.source.soul, SoulID::GoodsCompany(id)
            if owns_assignment(world, manifest.source, id, driver, truck))
    {
        return false;
    }

    let manifest = book.loads.remove(&load).unwrap();
    clear_assignment(world, &manifest, load);
    let mut market = resources.write::<Market>();
    let units = manifest.items.iter().map(|line| line.units).sum();
    for ItemQuantity { item, units } in manifest.items {
        let trade = if matches!(manifest.destination.soul, SoulID::FreightStation(_)) {
            market.freight_export(manifest.source.soul, manifest.destination.soul, item, units)
        } else {
            market.receive_freight(manifest.destination.soul, item, units);
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
    super::record_freight_outcome(resources, now, units, now > manifest.due_at);
    true
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
        book.routes
            .by_building
            .retain(|&(a, b), _| a != building && b != building);
    }
    let endpoint = |end: Endpoint| match removal {
        CargoRemoval::Building(id) => end.building == id,
        CargoRemoval::Soul(id) => end.soul == id,
        _ => false,
    };
    let shipments = book.shipments.iter()
        .filter_map(|(&id, s)| {
            let source_affected = match s.source.soul {
                SoulID::GoodsCompany(company) => world.companies.get(company).is_some_and(|entity| {
                    entity.comp.proto.prototype().storage.is_none()
                        && (matches!(removal, CargoRemoval::Driver(driver) if entity.comp.driver == Some(driver))
                            || matches!(removal, CargoRemoval::Truck(truck) if entity.comp.trucks.contains(&truck)))
                }),
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
    release(&mut book, world, resources, shipments, loads);
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
        super::record_freight_outcome(
            resources,
            now,
            load.items.iter().map(|line| line.units).sum(),
            true,
        );
    }
}

pub fn cargo_update(world: &mut World, resources: &mut Resources) {
    profiling::scope!("economy::cargo_update");
    let now = elapsed_ticks(resources).0;
    let mut book = resources.write::<CargoBook>();
    let match_due = now % FREIGHT_MATCH_INTERVAL == 0;
    if !match_due && book.loads.is_empty() && book.shipments.is_empty() {
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
    let shipments: Vec<_> = book
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
            let road_failed = outbound
                && (human.router.last_error.is_some()
                    || human.it.is_wait_for_reroute().is_some_and(|wait| wait > 0)
                    || (human.location == Location::Vehicle(load.vehicle)
                        && vehicle
                            .it
                            .is_wait_for_reroute()
                            .is_some_and(|wait| wait > 0)));
            (now >= load.departed_at + DELIVERY_EXPIRY
                || road_failed
                || !valid(&mut book.routes, load.source, load.destination)
                || !matches!(load.source.soul, SoulID::GoodsCompany(company)
                if owns_assignment(world, load.source, company, load.driver, load.vehicle))
                || !human.work.as_ref().is_some_and(|work| {
                    matches!(work.kind,
                WorkKind::Driver { load: Some(token), deliver_order: Some(dest), .. }
                if token == id && dest == load.destination())
                }))
            .then_some(id)
        })
        .collect();
    let released_sources = shipments
        .iter()
        .map(|id| book.shipments[id].source)
        .chain(loads.iter().map(|id| book.loads[id].source))
        .collect();
    release(book, world, resources, shipments, loads);
    if match_due {
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
    use crate::economy::DailyEconomyMetrics;
    use crate::map::BuildingKind;
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
        let proto = spec.id;
        let road = f.ctx.g.map().buildings()[f.destination.building].connected_road;
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
        assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, at));
        assert!(!complete_delivery(&mut f.ctx.g, token, driver, truck, at));
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
        assert!(!complete_delivery(
            &mut fixture.ctx.g,
            load,
            driver,
            truck,
            destination
        ));

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
        assert!(!complete_delivery(
            &mut fixture.ctx.g,
            load,
            driver,
            truck,
            destination
        ));
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
        let road = f.ctx.g.map().buildings()[f.destination.building].connected_road;
        let proto = prototypes::FreightStationPrototypeID::new("freight-station");
        let position = vec2(370.0, 467.0);
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
                road.filter(|_| connected),
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
                assert!(!complete_delivery(
                    &mut f.ctx.g,
                    token,
                    driver,
                    VehicleID::default(),
                    at
                ));
                f.ctx
                    .g
                    .write::<BuildingInfos>()
                    .set_owner(at, f.destination.soul);
                assert!(!complete_delivery(&mut f.ctx.g, token, driver, truck, at));
                f.ctx
                    .g
                    .write::<BuildingInfos>()
                    .set_owner(at, terminal.soul);
                assert!(complete_delivery(&mut f.ctx.g, token, driver, truck, at));
                assert_eq!(f.ctx.g.get(station).unwrap().f.waiting_cargo, 16);
                let expected = f
                    .ctx
                    .g
                    .read::<Market>()
                    .freight_export(f.source.soul, terminal.soul, Fixture::coal(), 16)
                    .money_delta;
                assert_eq!(export(&f) - before, i128::from(expected.0));
                assert!(!complete_delivery(&mut f.ctx.g, token, driver, truck, at));
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
}

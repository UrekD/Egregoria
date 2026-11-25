use crate::economy::cargo::{cancel_removed, stored_items, CargoBook, CargoRemoval, LoadId};
use geom::Vec3;
use itertools::Itertools;
use prototypes::{GameTime, GoodsCompanyID, ItemID, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::economy::{BasketFailure, Market, Trade};
use crate::map::{Building, BuildingID, BuildingKind, GrowableBusinessState, Map, PathKind};
use crate::map_dynamic::{
    BuildingInfos, DispatchID, Dispatcher, Itinerary, ParkingManagement, SpotReservation,
    NEAR_PARKING_RADIUS, OBJECTIVE_OK_DIST,
};
use crate::souls::desire::WorkKind;
use crate::souls::freight_station::FreightTrainState;
use crate::souls::household::{cancel_visit, household_items, HouseholdClaims};
use crate::souls::human::HumanDecisionKind;
use crate::souls::residence;
use crate::transportation::testing_vehicles::RandomVehicles;
use crate::transportation::train::{cancel_train_route, TrainReservations};
use crate::transportation::{
    put_pedestrian_in_transport_grid, spawn_parked_vehicle_with_spot, spawn_truck_at_depot,
    Location, TransportGrid, VehicleKind, VehicleState,
};
use crate::utils::resources::Resources;
use crate::{
    CompanyEnt, CompanyID, FreightStationEnt, HumanEnt, HumanID, ParCommandBuffer, Simulation,
    SoulID, TrainID, VehicleEnt, VehicleID, WagonEnt,
};

/// Queues a human and their owned vehicle for removal at the next command-buffer drain.
pub fn despawn_human(sim: &mut Simulation, id: HumanID) {
    sim.read::<ParCommandBuffer<HumanEnt>>().kill(id);
}

/// Durable inputs and transient cleanup for the company command-buffer drain.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct CompanyDepartures {
    queued: BTreeSet<CompanyID>,
    removed_sites: BTreeMap<BuildingID, Vec3>,
    #[serde(skip)]
    dropped: BTreeSet<SoulID>,
    #[serde(skip)]
    vacated_sites: BTreeSet<BuildingID>,
    #[serde(skip)]
    flush_scheduled: bool,
}

/// Queues a company and its fleet for removal at the next command-buffer drain.
pub fn despawn_company(sim: &mut Simulation, id: CompanyID) {
    if sim.world.companies.contains_key(id) && sim.world.company_departures.queued.insert(id) {
        sim.read::<ParCommandBuffer<CompanyEnt>>().kill(id);
    }
}

pub(crate) fn restore_company_departures(sim: &mut Simulation) {
    let pending = &sim.world.company_departures;
    {
        let buffer = sim.read::<ParCommandBuffer<CompanyEnt>>();
        for &company in &pending.queued {
            buffer.kill(company);
        }
    }
    if !pending.removed_sites.is_empty() {
        schedule_company_cleanup(sim);
    }
}

fn schedule_company_cleanup(sim: &mut Simulation) {
    if !sim.world.company_departures.flush_scheduled {
        sim.world.company_departures.flush_scheduled = true;
        sim.read::<ParCommandBuffer<CompanyEnt>>()
            .exec_ent(CompanyID::default(), finish_company_departures);
    }
}

fn finish_company_departures(sim: &mut Simulation) {
    let batch = &mut sim.world.company_departures;
    batch.flush_scheduled = false;
    let removed_sites = std::mem::take(&mut batch.removed_sites);
    let vacated_sites = std::mem::take(&mut batch.vacated_sites);
    let dropped = std::mem::take(&mut batch.dropped);
    let mut attached = Vec::new();
    if !removed_sites.is_empty() || !dropped.is_empty() {
        for (id, company) in sim.world.companies.iter_mut() {
            if removed_sites.contains_key(&company.comp.building) {
                attached.push(id);
            }
            if !dropped.is_empty() {
                let retain = |trade: &Trade| {
                    !dropped.contains(&trade.buyer.0) && !dropped.contains(&trade.seller.0)
                };
                company.sold.0.retain(retain);
                for trades in company.bought.0.values_mut() {
                    trades.retain(retain);
                }
            }
        }
    }
    if !dropped.is_empty() {
        sim.write::<Market>().remove_cached_trades(&dropped);
        sim.write::<BuildingInfos>().remove_soul_occupancy(&dropped);
    }
    if !removed_sites.is_empty() || !vacated_sites.is_empty() {
        let sites = vacated_sites
            .into_iter()
            .map(|id| (id, None))
            .chain(removed_sites.into_iter().map(|(id, door)| (id, Some(door))))
            .collect();
        clear_references(sim, &sites);
    }
    for company in attached {
        despawn_company(sim, company);
    }
}

/// Remove a site and settle all uses using the captured building record.
pub(crate) fn remove_site(sim: &mut Simulation, id: BuildingID) -> bool {
    let was_external_target = sim.map().external_train_stations.first() == Some(&id);
    let removed = sim.map_mut().remove_building(id);
    let Some(building) = removed else {
        return false;
    };
    residence::on_building_removed(sim, id);
    remove_building(sim, building, was_external_target);
    true
}

fn remove_building(sim: &mut Simulation, building: Building, was_external_target: bool) {
    let id = building.id;
    cancel_removed(sim, CargoRemoval::Building(id));
    if building.has_residential_use() {
        let (world, resources) = sim.world_res();
        let infos = resources.read::<BuildingInfos>();
        if let Some(info) = infos.get(id) {
            let mut market = resources.write::<Market>();
            for &resident in &info.residents {
                if let SoulID::Human(human) = resident {
                    if let Some(human) = world.humans.get_mut(human) {
                        market
                            .record_household_losses(std::mem::take(&mut human.household.on_hand));
                    }
                }
            }
        }
        drop(infos);
        residence::evict_residents(sim, id, building.door_pos);
    }
    sim.write::<BuildingInfos>().remove(id);
    match building.kind {
        BuildingKind::GoodsCompany(_) => {
            sim.world
                .company_departures
                .removed_sites
                .insert(id, building.door_pos);
            schedule_company_cleanup(sim);
            return;
        }
        BuildingKind::RailFreightStation(_) => {
            let buffer = sim.read::<ParCommandBuffer<FreightStationEnt>>();
            for (station, entity) in sim.world.freight_stations.iter() {
                if entity.f.building == id {
                    buffer.kill(station);
                }
            }
        }
        BuildingKind::ExternalTrading if was_external_target => {
            let no_external_target = sim.map().external_train_stations.is_empty();
            let (world, resources) = sim.world_res();
            let mut dispatcher = resources.write::<Dispatcher>();
            let mut reservations = resources.write::<TrainReservations>();
            for station in world.freight_stations.values_mut() {
                station.f.trains.retain(|&(id, state)| {
                    if no_external_target || matches!(state, FreightTrainState::Moving) {
                        dispatcher.free(id);
                        if let Some(train) = world.trains.get_mut(id) {
                            cancel_train_route(id, train, &mut reservations);
                        }
                        false
                    } else {
                        true
                    }
                });
            }
        }
        _ => {}
    }
    clear_building_references(sim, id, Some(building.door_pos));
}

pub(crate) fn human_dropped(sim: &mut Simulation, id: HumanID, human: &mut HumanEnt) {
    cancel_removed(sim, CargoRemoval::Driver(id));
    let (_, resources) = sim.world_res();
    human
        .household
        .cancel(resources, BasketFailure::ShopperRemoved);
    human.household.discard_order(resources);
    cancel_visit(id, human, resources);
    {
        let mut market = sim.write::<Market>();
        market.record_household_losses(
            std::mem::take(&mut human.household.on_hand)
                .into_iter()
                .chain(std::mem::take(&mut human.household.carried)),
        );
        for trade in human.bought.0.values().flatten() {
            market.forget_human_buyer(trade.seller.0, id);
        }
    }
    if let Some(car) = human.router.personal_car {
        sim.read::<ParCommandBuffer<VehicleEnt>>().kill(car);
    }
    human
        .router
        .clear_steps(&mut sim.write::<ParkingManagement>());
    if let Some(trades) = human.bought.0.remove(&ItemID::new("retail-goods")) {
        let map = sim.map();
        let infos = sim.read::<BuildingInfos>();
        let mut market = sim.write::<Market>();
        for trade in trades {
            let seller_pos = infos
                .building_owned_by(trade.seller.0)
                .and_then(|building| map.get(building))
                .map(|store| store.door_pos.xy());
            market.cancel_retail_purchase(trade, seller_pos);
        }
    }
    let employers = clear_soul_references(sim, SoulID::Human(id));
    if let (Some(_), Some(car), Some(batch)) = (
        &employers,
        human.router.personal_car,
        &mut sim.world.departing,
    ) {
        batch.removed_cars.insert(car);
    }

    if let Some(WorkKind::Driver { truck, .. }) = human.work.as_ref().map(|work| work.kind) {
        sim.write::<Dispatcher>()
            .free(DispatchID::SmallTruck(truck));
        return_truck_to_depot(sim, truck);
    }
    let (world, resources) = sim.world_res();
    let map = resources.read::<Map>();
    let mut market = resources.write::<Market>();
    let job = ItemID::new("job-opening");
    let mut remove_worker = |company, entity: &mut CompanyEnt| {
        if entity.comp.driver == Some(id) {
            entity.comp.driver = None;
        }
        let worker_count = entity.workers.0.len();
        entity.workers.0.retain(|&worker| worker != id);
        if entity.workers.0.len() == worker_count {
            return;
        }

        if let Some(building) = map.get(entity.comp.building) {
            let soul = SoulID::GoodsCompany(company);
            market.produce(soul, job, 1);
            market.sell_all(soul, building.door_pos.xy(), job, 0);
        }
    };
    if let Some(employers) = employers {
        for company in employers {
            if let Some(entity) = world.companies.get_mut(company) {
                remove_worker(company, entity);
            }
        }
    } else {
        for (company, entity) in world.companies.iter_mut() {
            remove_worker(company, entity);
        }
    }
}

/// Mark a profiled company unavailable before draining the queued closure.
pub fn close_business(sim: &mut Simulation, id: CompanyID) -> bool {
    let Some(company) = sim.get(id) else {
        return false;
    };
    let building = company.comp.building;
    if !mark_business_closed(sim, id, building) {
        return false;
    }
    despawn_company(sim, id);
    true
}

fn mark_business_closed(sim: &Simulation, id: CompanyID, building: BuildingID) -> bool {
    if sim.read::<BuildingInfos>().owner(building) != Some(SoulID::GoodsCompany(id)) {
        return false;
    }
    let elapsed = crate::map_dynamic::transit::elapsed_ticks(&sim.resources).0;
    let mut map = sim.map_mut();
    let open = map
        .get(building)
        .and_then(Building::development)
        .is_some_and(|site| site.business == Some(GrowableBusinessState::Open));
    if open {
        map.set_growable_business_state(
            building,
            GrowableBusinessState::ClosedUntil {
                retry_elapsed_tick: elapsed.saturating_add(24 * TICKS_PER_HOUR),
            },
        );
    }
    open
}

/// Mark the site before its company departs. The residence leaf settles the marked homes as a batch.
pub fn mark_site_abandoned(sim: &mut Simulation, building: BuildingID) -> bool {
    let now = crate::map_dynamic::transit::elapsed_ticks(&sim.resources).0;
    let marked = sim.map_mut().mark_growable_abandoned(building, now);
    if marked {
        let owner = sim.read::<BuildingInfos>().owner(building);
        if let Some(SoulID::GoodsCompany(company)) = owner {
            close_business(sim, company);
        }
    }
    marked
}

pub(crate) fn company_dropped(sim: &mut Simulation, id: CompanyID, company: CompanyEnt) {
    let building = company.comp.building;
    let (profiled, shared_address, site_survives) = sim
        .map()
        .get(building)
        .map_or((false, false, false), |site| {
            (site.development().is_some(), site.is_mixed_use(), true)
        });
    sim.world.company_departures.queued.remove(&id);
    if profiled {
        mark_business_closed(sim, id, building);
    }
    cancel_removed(sim, CargoRemoval::Soul(SoulID::GoodsCompany(id)));
    sim.write::<CargoBook>().forget_company(id);
    write_off_physical_stock(sim, SoulID::GoodsCompany(id), company.comp.proto);
    sim.read::<ParCommandBuffer<VehicleEnt>>()
        .kill_all(&company.comp.trucks);
    // A hire can be matched before the company's update assigns the workplace.
    // Its worker roster already contains these residents.
    {
        let (world, resources) = sim.world_res();
        let mut market = resources.write::<Market>();
        let job = ItemID::new("job-opening");
        let seller = SoulID::GoodsCompany(id);
        for &worker in &company.workers.0 {
            let Some(human) = world.humans.get_mut(worker) else {
                continue;
            };
            let Some(orders) = human.bought.0.get_mut(&job) else {
                continue;
            };
            orders.retain(|trade| trade.seller.0 != seller);
            if orders.is_empty() {
                human.bought.0.remove(&job);
                if human.work.is_none() {
                    market.buy(SoulID::Human(worker), human.trans.pos.xy(), job, 1);
                }
            }
        }
    }
    clear_soul_references(sim, SoulID::GoodsCompany(id));
    if shared_address {
        clear_company_workers(sim, id, &company);
    } else if site_survives {
        sim.world.company_departures.vacated_sites.insert(building);
    }
    schedule_company_cleanup(sim);
}

fn clear_company_workers(sim: &mut Simulation, id: CompanyID, company: &CompanyEnt) {
    let (world, resources) = sim.world_res();
    let mut parking = resources.write::<ParkingManagement>();
    for &worker in &company.workers.0 {
        let Some(human) = world.humans.get_mut(worker) else {
            continue;
        };
        if !human
            .work
            .as_ref()
            .is_some_and(|work| work.employer == Some(id))
        {
            continue;
        }

        human.work = None;
        human.household.attended_shift = None;
        resources.write::<Market>().buy(
            SoulID::Human(worker),
            human.trans.pos.xy(),
            ItemID::new("job-opening"),
            1,
        );
        if human.router.commutes_for(id) {
            cancel_trip(worker, human, resources, &mut parking);
        } else if human.decision.kind.references_employer(id) {
            human.decision = Default::default();
        }
    }
}

/// Physical stock leaves with its company. Record it once in the itemized
/// write-off ledger before the market account goes. Household shelves and shop
/// deliveries are written off when the account itself is removed.
fn write_off_physical_stock(sim: &mut Simulation, soul: SoulID, proto: GoodsCompanyID) {
    let proto = proto.prototype();
    let mut items: BTreeSet<ItemID> = proto.storage.as_ref().map(stored_items).unwrap_or_default();
    if let Some(recipe) = &proto.recipe {
        items.extend(
            recipe
                .consumption
                .iter()
                .chain(&recipe.production)
                .map(|item| item.id),
        );
    }
    let delivery = crate::economy::balance::shelf_delivery_item();
    let mut market = sim.write::<Market>();
    for item in items {
        if household_items().contains(&item) || item == delivery {
            continue;
        }
        let stock = market.capital(soul, item);
        if stock > 0 {
            market.record_freight_loss(item, stock as u32);
            market.produce(soul, item, -stock);
        }
    }
}

pub(crate) fn freight_station_dropped(
    sim: &mut Simulation,
    id: crate::FreightStationID,
    station: FreightStationEnt,
) {
    cancel_removed(sim, CargoRemoval::Soul(SoulID::FreightStation(id)));
    clear_soul_references(sim, SoulID::FreightStation(id));
    clear_building_references(sim, station.f.building, None);
    let (world, resources) = sim.world_res();
    let mut dispatcher = resources.write::<Dispatcher>();
    let mut reservations = resources.write::<TrainReservations>();
    for (id, _) in station.f.trains {
        dispatcher.free(id);
        if let Some(train) = world.trains.get_mut(id) {
            cancel_train_route(id, train, &mut reservations);
        }
    }
}

pub(crate) fn vehicle_dropped(sim: &mut Simulation, id: VehicleID, position: Vec3) {
    cancel_removed(sim, CargoRemoval::Truck(id));
    detach_transit_vehicle(sim, id, position);
    let (world, resources) = sim.world_res();
    resources.write::<RandomVehicles>().vehicles.remove(&id);
    let indexed = world.departing.as_mut().and_then(|batch| {
        if !batch.removed_cars.contains(&id) {
            return None;
        }
        let references = batch.vehicle_references.get_or_insert_with(|| {
            VehicleDepartureReferences::new(&batch.removed_cars, &world.humans, &world.companies)
        });
        references
            .by_vehicle
            .remove(&id)
            .map(|entry| (entry, &references.active_routes))
    });

    let mut parking = resources.write::<ParkingManagement>();
    let mut former_drivers = BTreeSet::new();
    let mut clear_human = |human_id, human: &mut HumanEnt| {
        let riding = human.location == Location::Vehicle(id);
        if riding {
            move_outside(human, position, resources);
        }
        let mut lost_truck = false;
        if let Some(work) = &mut human.work {
            if matches!(work.kind, WorkKind::Driver { truck, .. } if truck == id) {
                former_drivers.insert(human_id);
                work.kind = WorkKind::Worker;
                lost_truck = true;
            }
        }

        if human.router.forget_vehicle(id)
            || riding
            || lost_truck
            || human.decision.kind.references_vehicle(id)
        {
            cancel_trip(human_id, human, resources, &mut parking);
        }
    };

    if let Some((references, active_routes)) = &indexed {
        for &(_, human_id) in references
            .humans
            .iter()
            .merge_by(active_routes.iter(), |a, b| a.0 < b.0)
        {
            if let Some(human) = world.humans.get_mut(human_id) {
                clear_human(human_id, human);
            }
        }
    } else {
        for (human_id, human) in world.humans.iter_mut() {
            clear_human(human_id, human);
        }
    }

    let clear_company = |company: &mut CompanyEnt| {
        company.comp.trucks.retain(|&truck| truck != id);
        if company
            .comp
            .driver
            .is_some_and(|driver| former_drivers.contains(&driver))
        {
            company.comp.driver = None;
        }
    };
    if let Some((references, _)) = indexed {
        for company in references.companies {
            if let Some(company) = world.companies.get_mut(company) {
                clear_company(company);
            }
        }
    } else {
        for company in world.companies.values_mut() {
            clear_company(company);
        }
    }
}

pub(crate) fn train_dropped(sim: &mut Simulation, id: TrainID) {
    let (world, resources) = sim.world_res();
    resources
        .write::<Dispatcher>()
        .unregister(DispatchID::FreightTrain(id));
    let mut reservations = resources.write::<TrainReservations>();
    reservations.reservations.retain(|_, train| *train != id);
    reservations.localisations.retain(|_, trains| {
        trains.remove(&id);
        !trains.is_empty()
    });
    let wagons = resources.read::<ParCommandBuffer<WagonEnt>>();
    for (wagon, entity) in world.wagons.iter() {
        if entity.itfollower.leader == id {
            wagons.kill(wagon);
        }
    }
    for station in world.freight_stations.values_mut() {
        station.f.trains.retain(|&(train, _)| train != id);
    }
}

#[derive(Default)]
pub(crate) struct DepartureReferences {
    pending: BTreeMap<HumanID, Departure>,
    removed_souls: BTreeSet<SoulID>,
    removed_cars: BTreeSet<VehicleID>,
    vehicle_references: Option<VehicleDepartureReferences>,
}

#[derive(Default)]
struct Departure {
    human_buyers: Vec<HumanID>,
    company_traders: Vec<CompanyID>,
    employers: Vec<CompanyID>,
}

struct VehicleDepartureReferences {
    by_vehicle: BTreeMap<VehicleID, VehicleReferences>,
    active_routes: Vec<(usize, HumanID)>,
}

#[derive(Default)]
struct VehicleReferences {
    humans: Vec<(usize, HumanID)>,
    companies: Vec<CompanyID>,
}

impl VehicleDepartureReferences {
    fn new(
        vehicles: &BTreeSet<VehicleID>,
        humans: &slotmapd::HopSlotMap<HumanID, HumanEnt>,
        companies: &slotmapd::HopSlotMap<CompanyID, CompanyEnt>,
    ) -> Self {
        let mut references: BTreeMap<_, VehicleReferences> = vehicles
            .iter()
            .map(|&id| (id, VehicleReferences::default()))
            .collect();
        let mut active_routes = Vec::new();
        for (order, (id, human)) in humans.iter().enumerate() {
            if human.router.target_dest.is_some() || human.router.cur_step().is_some() {
                // An active route can still contain steps for an earlier vehicle
                // even after the selected vehicle changes.
                active_routes.push((order, id));
                continue;
            }
            let mut mentioned = BTreeSet::new();
            mentioned.extend(human.router.personal_car);
            mentioned.extend(human.router.selected_vehicle());
            if let Location::Vehicle(vehicle) = human.location {
                mentioned.insert(vehicle);
            }
            mentioned.extend(driver_truck(human));
            decision_vehicles(&human.decision.kind, &mut mentioned);
            for vehicle in mentioned {
                if let Some(refs) = references.get_mut(&vehicle) {
                    refs.humans.push((order, id));
                }
            }
        }
        for (id, company) in companies.iter() {
            let mut mentioned: BTreeSet<_> = company.comp.trucks.iter().copied().collect();
            if let Some(driver) = company.comp.driver.and_then(|driver| humans.get(driver)) {
                mentioned.extend(driver_truck(driver));
            }
            for vehicle in mentioned {
                if let Some(refs) = references.get_mut(&vehicle) {
                    refs.companies.push(id);
                }
            }
        }
        Self {
            by_vehicle: references,
            active_routes,
        }
    }
}

fn driver_truck(human: &HumanEnt) -> Option<VehicleID> {
    match human.work.as_ref()?.kind {
        WorkKind::Driver { truck, .. } => Some(truck),
        WorkKind::Worker => None,
    }
}

fn decision_vehicles(decision: &HumanDecisionKind, vehicles: &mut BTreeSet<VehicleID>) {
    match decision {
        HumanDecisionKind::SetVehicle(Some(id)) => {
            vehicles.insert(*id);
        }
        HumanDecisionKind::MultiStack(steps) => {
            for step in steps {
                decision_vehicles(step, vehicles);
            }
        }
        _ => {}
    }
}

fn human_trade_participants<'a>(trades: impl Iterator<Item = &'a Trade>) -> BTreeSet<HumanID> {
    trades
        .flat_map(|trade| [trade.buyer.0, trade.seller.0])
        .filter_map(|soul| match soul {
            SoulID::Human(id) => Some(id),
            _ => None,
        })
        .collect()
}

fn clear_soul_references(sim: &mut Simulation, soul: SoulID) -> Option<Vec<CompanyID>> {
    let (world, resources) = sim.world_res();
    let seller = !matches!(soul, SoulID::Human(_));
    if seller {
        let shoppers = resources.write::<HouseholdClaims>().baskets.remove(&soul);
        if let Some(shoppers) = shoppers {
            let mut parking = resources.write::<ParkingManagement>();
            for id in shoppers {
                if let Some(human) = world.humans.get_mut(id) {
                    human
                        .household
                        .cancel(resources, BasketFailure::SellerRemoved);
                    human.router.walk_only_pickup = false;
                    cancel_trip(id, human, resources, &mut parking);
                }
            }
        }
    }
    let mut market = resources.write::<Market>();
    let legacy_buyers = if seller {
        market.index_human_buyers(world);
        market.take_human_buyers(soul)
    } else {
        BTreeSet::new()
    };
    let mut infos = resources.write::<BuildingInfos>();
    let departure = match (soul, &mut world.departing) {
        (SoulID::Human(id), Some(batch)) => batch.pending.remove(&id).inspect(|_| {
            batch.removed_souls.insert(soul);
        }),
        _ => None,
    };
    let company_departure = matches!(soul, SoulID::GoodsCompany(_));
    if company_departure {
        world.company_departures.dropped.insert(soul);
    }
    if departure.is_some() || company_departure {
        // Keep account and ownership changes in drop order. Only the shared
        // trade buffers and building occupancy lists can wait for the batch.
        market.remove_account(soul);
        infos.remove_soul_affiliations(soul);
    } else {
        market.remove(soul);
        infos.remove_soul(soul);
    }
    drop(infos);
    let bread = ItemID::new("bread");
    let retail = ItemID::new("retail-goods");
    let mut remove_human_trades = |human: &mut HumanEnt| {
        human.bought.0.retain(|&item, trades| {
            let count = trades.len();
            trades.retain(|trade| {
                if !trade.involves(soul) {
                    return true;
                }
                if item == retail {
                    market.cancel_retail_purchase(*trade, None);
                }
                false
            });
            if item == bread && count != 0 && trades.is_empty() {
                human.food.cancel_purchase();
            }
            !trades.is_empty() || count == 0
        });
    };
    let remove_company_trades = |company: &mut CompanyEnt| {
        company.sold.0.retain(|trade| !trade.involves(soul));
        for trades in company.bought.0.values_mut() {
            trades.retain(|trade| !trade.involves(soul));
        }
    };
    if let Some(entry) = departure {
        for buyer in entry.human_buyers {
            if let Some(human) = world.humans.get_mut(buyer) {
                remove_human_trades(human);
            }
        }
        for company in entry.company_traders {
            if let Some(company) = world.companies.get_mut(company) {
                remove_company_trades(company);
            }
        }
        return Some(entry.employers);
    }
    if seller {
        for buyer in legacy_buyers {
            if let Some(human) = world.humans.get_mut(buyer) {
                remove_human_trades(human);
            }
        }
    } else {
        for human in world.humans.values_mut() {
            remove_human_trades(human);
        }
    }
    if !company_departure {
        for company in world.companies.values_mut() {
            remove_company_trades(company);
        }
    }
    None
}

pub(crate) fn clear_building_references(sim: &mut Simulation, id: BuildingID, door: Option<Vec3>) {
    clear_references(sim, &BTreeMap::from([(id, door)]));
}

pub(crate) fn clear_abandoned_home_references(sim: &mut Simulation, homes: &[(BuildingID, Vec3)]) {
    let buildings = homes.iter().map(|&(id, door)| (id, Some(door))).collect();
    clear_references(sim, &buildings);

    let (world, resources) = sim.world_res();
    let infos = resources.read::<BuildingInfos>();
    // Eviction has already removed or reassigned residence memberships. Only
    // actual command-buffer deletions will consume entries from this index.
    let mut departing: BTreeMap<_, Departure> = world
        .humans
        .iter()
        .filter(|(id, human)| {
            buildings.contains_key(&human.home.house)
                && infos.residence_of(SoulID::Human(*id)).is_none()
        })
        .map(|(id, _)| (id, Departure::default()))
        .collect();
    if departing.len() < 2 {
        return;
    }
    for (buyer, human) in world.humans.iter() {
        for id in human_trade_participants(human.bought.0.values().flatten()) {
            if let Some(entry) = departing.get_mut(&id) {
                entry.human_buyers.push(buyer);
            }
        }
    }
    for (company, entity) in world.companies.iter() {
        let trades = entity
            .sold
            .0
            .iter()
            .chain(entity.bought.0.values().flatten());
        for id in human_trade_participants(trades) {
            if let Some(entry) = departing.get_mut(&id) {
                entry.company_traders.push(company);
            }
        }
        let mut workers: BTreeSet<_> = entity.workers.0.iter().copied().collect();
        workers.extend(entity.comp.driver);
        for id in workers {
            if let Some(entry) = departing.get_mut(&id) {
                entry.employers.push(company);
            }
        }
    }
    debug_assert!(world.departing.is_none());
    world.departing = Some(DepartureReferences {
        pending: departing,
        ..DepartureReferences::default()
    });
    resources
        .read::<ParCommandBuffer<HumanEnt>>()
        .exec_ent(HumanID::default(), finish_human_departures);
}

fn finish_human_departures(sim: &mut Simulation) {
    let (world, resources) = sim.world_res();
    let mut batch = world.departing.take().expect("active departure batch");
    if !batch.removed_souls.is_empty() {
        resources
            .write::<Market>()
            .remove_cached_trades(&batch.removed_souls);
        resources
            .write::<BuildingInfos>()
            .remove_soul_occupancy(&batch.removed_souls);
    }
    batch.pending.clear();
    batch.removed_souls.clear();
    if batch.removed_cars.len() > 1 {
        world.departing = Some(batch);
        resources
            .read::<ParCommandBuffer<VehicleEnt>>()
            .exec_ent(VehicleID::default(), |sim| {
                sim.world.departing = None;
            });
    }
}

fn clear_references(sim: &mut Simulation, buildings: &BTreeMap<BuildingID, Option<Vec3>>) {
    let (world, resources) = sim.world_res();
    let mut parking = resources.write::<ParkingManagement>();
    let mut returning_trucks = BTreeSet::new();
    let mut idle_drivers = BTreeSet::new();
    for (human_id, human) in world.humans.iter_mut() {
        let door = match human.location {
            Location::Building(id) => buildings.get(&id).copied().flatten(),
            _ => None,
        };
        for &id in buildings.keys() {
            human.food.forget_building(id);
        }

        let seller_removed = human
            .household
            .reservation
            .as_ref()
            .is_some_and(|basket| buildings.contains_key(&basket.building));
        if seller_removed {
            human
                .household
                .cancel(resources, BasketFailure::SellerRemoved);
            human.router.walk_only_pickup = false;
        }
        let mut cancel = door.is_some() || seller_removed;
        if let Some(work) = &mut human.work {
            if buildings.contains_key(&work.workplace) {
                human.work = None;
                human.household.attended_shift = None;
                resources.write::<Market>().buy(
                    SoulID::Human(human_id),
                    human.trans.pos.xy(),
                    ItemID::new("job-opening"),
                    1,
                );
                cancel = true;
            } else if let WorkKind::Driver {
                deliver_order,
                truck,
                ..
            } = &mut work.kind
            {
                if deliver_order.is_some_and(|id| buildings.contains_key(&id)) {
                    *deliver_order = None;
                    resources
                        .write::<Dispatcher>()
                        .free(DispatchID::SmallTruck(*truck));
                    returning_trucks.insert(*truck);
                    idle_drivers.insert(human_id);
                    work.kind = WorkKind::Worker;
                    cancel = true;
                }
            }
        }

        if let Some(door) = door {
            move_outside(human, door, resources);
        }
        let includes = |id| buildings.contains_key(&id);
        cancel = cancel
            || human.router.references_any_building(includes)
            || human.decision.kind.references_any_building(includes);
        if cancel {
            if let Location::Vehicle(vehicle) = human.location {
                if let Some(body) = world.vehicles.get_mut(vehicle) {
                    if matches!(body.vehicle.kind, VehicleKind::Truck) {
                        returning_trucks.insert(vehicle);
                    } else {
                        body.it = Itinerary::NONE;
                    }
                }
            }
            cancel_trip(human_id, human, resources, &mut parking);
        }
    }
    drop(parking);
    {
        let mut infos = resources.write::<BuildingInfos>();
        for (&id, door) in buildings {
            if let Some(info) = infos.get_mut(id).filter(|_| door.is_some()) {
                info.inside.clear();
            }
        }
    }
    if !idle_drivers.is_empty() {
        for company in world.companies.values_mut() {
            if company
                .comp
                .driver
                .is_some_and(|driver| idle_drivers.contains(&driver))
            {
                company.comp.driver = None;
            }
        }
    }
    for truck in returning_trucks {
        return_truck_to_depot(sim, truck);
    }
}

fn truck_company(sim: &Simulation, truck: VehicleID) -> Option<CompanyID> {
    sim.world
        .companies
        .iter()
        .find_map(|(id, company)| company.comp.trucks.contains(&truck).then_some(id))
}

pub(crate) fn abort_freight_trip(
    sim: &mut Simulation,
    driver: HumanID,
    truck: VehicleID,
    token: LoadId,
    used_truck: bool,
) {
    if sim.read::<CargoBook>().manifest_for(truck).is_some() {
        return;
    }
    stop_freight_driver(sim, driver, truck, |human| {
        human.decision.kind.references_load(token)
    });
    recall_truck(sim, truck, used_truck);
}

/// Brings an empty truck back to its depot when its return leg kept failing or
/// its driver left. A driver still on a trip with the truck is freed; any other
/// trip of theirs goes on. A truck parked away from the depot is replaced there.
pub(crate) fn recover_empty_truck(sim: &mut Simulation, driver: HumanID, truck: VehicleID) {
    if sim.read::<CargoBook>().manifest_for(truck).is_some() {
        return;
    }
    stop_freight_driver(sim, driver, truck, |human| {
        human.router.selected_vehicle() == Some(truck)
            || human.decision.kind.references_vehicle(truck)
    });
    let depot = truck_company(sim, truck).and_then(|company| {
        sim.map()
            .get(sim.get(company)?.comp.building)
            .map(|b| b.door_pos)
    });
    let away = sim.get(truck).is_some_and(|vehicle| {
        depot.is_none_or(|door| vehicle.trans.pos.distance(door) > NEAR_PARKING_RADIUS)
    });
    recall_truck(sim, truck, away);
}

fn stop_freight_driver(
    sim: &mut Simulation,
    driver: HumanID,
    truck: VehicleID,
    on_trip: impl Fn(&HumanEnt) -> bool,
) {
    let (world, resources) = sim.world_res();
    let Some(human) = world.humans.get_mut(driver) else {
        return;
    };
    let riding = human.location == Location::Vehicle(truck);
    if riding {
        let pos = world
            .vehicles
            .get(truck)
            .map_or(human.trans.pos, |v| v.trans.pos);
        move_outside(human, pos, resources);
    }
    if riding || on_trip(human) {
        if human.router.selected_vehicle() == Some(truck) {
            human.router.use_vehicle(human.router.personal_car);
        }
        cancel_trip(
            driver,
            human,
            resources,
            &mut resources.write::<ParkingManagement>(),
        );
    }
}

/// Returns a truck to its depot. A truck that is parked or parking away from
/// the depot is replaced by a new one there.
fn recall_truck(sim: &mut Simulation, truck: VehicleID, away: bool) {
    if away
        && sim.get(truck).is_some_and(|vehicle| {
            matches!(
                vehicle.vehicle.state,
                VehicleState::Parked(_) | VehicleState::RoadToPark(..)
            )
        })
    {
        if let Some(company) = truck_company(sim, truck) {
            let depot = sim.get(company).unwrap().comp.building;
            let door = sim.map().get(depot).map(|building| building.door_pos);
            if let Some(door) = door {
                let replacement = spawn_truck_at_depot(sim, door);
                replace_truck(sim, company, truck, replacement);
                return;
            }
        }
    }
    return_truck_to_depot(sim, truck);
}

fn return_truck_to_depot(sim: &mut Simulation, truck: VehicleID) {
    let Some(vehicle) = sim.get(truck) else {
        return;
    };
    if !matches!(
        vehicle.vehicle.state,
        VehicleState::Driving | VehicleState::Panicking(_)
    ) {
        return;
    }
    let Some(company) = truck_company(sim, truck) else {
        return;
    };
    let map = sim.map();
    let building = sim.get(company).unwrap().comp.building;
    let Some(depot) = map.get(building) else {
        return; // Removing the company also removes its fleet.
    };
    let door = depot.door_pos;
    let spot = sim.write::<ParkingManagement>().reserve_near(door, &map);
    let Ok(spot) = spot else {
        drop(map);
        let replacement = spawn_truck_at_depot(sim, door);
        replace_truck(sim, company, truck, replacement);
        return;
    };
    let tick = sim.read::<GameTime>().tick;
    let route = spot.park_pos(&map).and_then(|target| {
        Itinerary::route(tick, vehicle.trans.pos, target, &map, PathKind::Vehicle)
    });
    drop(map);
    if let Some(route) = route {
        let vehicle = sim.world.vehicles.get_mut(truck).unwrap();
        vehicle.it = route;
        vehicle.vehicle.state = VehicleState::ReturningToDepot {
            spot,
            stopped_ticks: 0,
        };
    } else {
        respawn_truck_at_depot(sim, truck, spot);
    }
}

pub(crate) fn finish_truck_return(sim: &mut Simulation, truck: VehicleID) {
    let Some(vehicle) = sim.world.vehicles.get_mut(truck) else {
        return;
    };
    let state = std::mem::replace(&mut vehicle.vehicle.state, VehicleState::Driving);
    let VehicleState::ReturningToDepot { spot, .. } = state else {
        vehicle.vehicle.state = state;
        return;
    };

    let map = sim.resources.read::<Map>();
    let Some(parking) = spot.get(&map.parking) else {
        drop(map);
        sim.write::<ParkingManagement>().free(spot);
        return_truck_to_depot(sim, truck);
        return;
    };
    let arrived = vehicle.it.has_ended(0.0)
        && spot
            .park_pos(&map)
            .is_some_and(|pos| pos.distance(vehicle.trans.pos) <= 2.0 * OBJECTIVE_OK_DIST);
    if arrived {
        vehicle.it = Itinerary::NONE;
        vehicle.begin_parking(parking.trans, spot);
    } else {
        drop(map);
        respawn_truck_at_depot(sim, truck, spot);
    }
}

fn respawn_truck_at_depot(sim: &mut Simulation, truck: VehicleID, spot: SpotReservation) {
    let Some(company) = truck_company(sim, truck) else {
        sim.write::<ParkingManagement>().free(spot);
        sim.read::<ParCommandBuffer<VehicleEnt>>().kill(truck);
        return;
    };
    let replacement = spawn_parked_vehicle_with_spot(sim, VehicleKind::Truck, spot)
        .expect("reserved depot parking spot");
    replace_truck(sim, company, truck, replacement);
}

fn replace_truck(sim: &mut Simulation, company: CompanyID, old: VehicleID, new: VehicleID) {
    let company = sim.world.companies.get_mut(company).unwrap();
    let truck = company
        .comp
        .trucks
        .iter_mut()
        .find(|truck| **truck == old)
        .unwrap();
    *truck = new;
    // Its driver keeps the job with the new truck, so removing the old one does not
    // cancel whatever trip they are on now.
    for &driver in company.workers.0.iter().chain(&company.comp.driver) {
        let Some(work) = sim
            .world
            .humans
            .get_mut(driver)
            .and_then(|human| human.work.as_mut())
        else {
            continue;
        };
        if let WorkKind::Driver { truck, .. } = &mut work.kind {
            if *truck == old {
                *truck = new;
            }
        }
    }
    sim.read::<ParCommandBuffer<VehicleEnt>>().kill(old);
}

fn move_outside(human: &mut HumanEnt, pos: Vec3, resources: &Resources) {
    let mut grid = resources.write::<TransportGrid>();
    if let Some(collider) = human.collider.take() {
        grid.remove_maintain(collider.0);
    }
    human.collider = Some(put_pedestrian_in_transport_grid(&mut grid, pos));
    human.location = Location::Outside;
    human.trans.pos = pos;
    human.speed.0 = 0.0;
}

fn cancel_trip(
    id: HumanID,
    human: &mut HumanEnt,
    resources: &Resources,
    parking: &mut ParkingManagement,
) {
    cancel_visit(id, human, resources);
    human
        .household
        .cancel(resources, BasketFailure::RouteInterrupted);
    human.household.homebound_until = None;
    human.router.walk_only_pickup = false;
    if crate::map_dynamic::transit::on_route_cancelled(human, resources) {
        return;
    }
    human.router.cancel_route(parking);
    human.it = Itinerary::NONE;
    human.decision = Default::default();
}

fn detach_transit_vehicle(sim: &mut Simulation, id: VehicleID, position: Vec3) {
    crate::map_dynamic::transit::on_vehicle_dropped(sim, id, position);
}

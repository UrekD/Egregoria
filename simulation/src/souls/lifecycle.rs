use geom::Vec3;
use prototypes::ItemID;

use crate::economy::Market;
use crate::map::{Building, BuildingID, BuildingKind, Map};
use crate::map_dynamic::{BuildingInfos, DispatchID, Dispatcher, Itinerary, ParkingManagement};
use crate::souls::desire::WorkKind;
use crate::souls::freight_station::FreightTrainState;
use crate::transportation::testing_vehicles::RandomVehicles;
use crate::transportation::train::{cancel_train_route, TrainReservations};
use crate::transportation::{put_pedestrian_in_transport_grid, Location, TransportGrid};
use crate::utils::resources::Resources;
use crate::{
    CompanyEnt, CompanyID, FreightStationEnt, HumanEnt, HumanID, ParCommandBuffer, Simulation,
    SoulID, TrainID, VehicleEnt, VehicleID, WagonEnt,
};

/// Queues a human and their owned vehicle for removal at the next command-buffer drain.
pub fn despawn_human(sim: &mut Simulation, id: HumanID) {
    sim.read::<ParCommandBuffer<HumanEnt>>().kill(id);
}

/// Queues a company and its fleet for removal at the next command-buffer drain.
pub fn despawn_company(sim: &mut Simulation, id: CompanyID) {
    sim.read::<ParCommandBuffer<CompanyEnt>>().kill(id);
}

pub(crate) fn remove_building(sim: &mut Simulation, building: Building, was_external_target: bool) {
    let id = building.id;
    sim.write::<BuildingInfos>().remove(id);
    match building.kind {
        BuildingKind::House => {
            let residents: Vec<_> = sim
                .world
                .humans
                .iter()
                .filter_map(|(human, body)| (body.home.house == id).then_some(human))
                .collect();
            for resident in residents {
                despawn_human(sim, resident);
            }
        }
        BuildingKind::GoodsCompany(_) => {
            let companies: Vec<_> = sim
                .world
                .companies
                .iter()
                .filter_map(|(id, comp)| (comp.comp.building == building.id).then_some(id))
                .collect();
            for company in companies {
                despawn_company(sim, company);
            }
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
    if let Some(car) = human.router.personal_car {
        sim.read::<ParCommandBuffer<VehicleEnt>>().kill(car);
    }
    human
        .router
        .clear_steps(&mut sim.write::<ParkingManagement>());
    clear_soul_references(sim, SoulID::Human(id));

    let (world, resources) = sim.world_res();
    if let Some(WorkKind::Driver { truck, .. }) = human.work.as_ref().map(|work| work.kind) {
        resources
            .write::<Dispatcher>()
            .free(DispatchID::SmallTruck(truck));
        if let Some(vehicle) = world.vehicles.get_mut(truck) {
            vehicle.it = Itinerary::NONE;
        }
    }
    let map = resources.read::<Map>();
    let mut market = resources.write::<Market>();
    let job = ItemID::new("job-opening");
    for (company, entity) in world.companies.iter_mut() {
        if entity.comp.driver == Some(id) {
            entity.comp.driver = None;
        }
        let worker_count = entity.workers.0.len();
        entity.workers.0.retain(|&worker| worker != id);
        if entity.workers.0.len() < worker_count {
            if let Some(building) = map.get(entity.comp.building) {
                let soul = SoulID::GoodsCompany(company);
                market.produce(soul, job, 1);
                market.sell_all(soul, building.door_pos.xy(), job, 0);
            }
        }
    }
}

pub(crate) fn company_dropped(sim: &mut Simulation, id: CompanyID, company: CompanyEnt) {
    sim.read::<ParCommandBuffer<VehicleEnt>>()
        .kill_all(&company.comp.trucks);
    clear_soul_references(sim, SoulID::GoodsCompany(id));
    clear_building_references(sim, company.comp.building, None);
}

pub(crate) fn freight_station_dropped(
    sim: &mut Simulation,
    id: crate::FreightStationID,
    station: FreightStationEnt,
) {
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
    let (world, resources) = sim.world_res();
    resources.write::<RandomVehicles>().vehicles.remove(&id);
    let mut former_drivers = Vec::new();
    for (human_id, human) in world.humans.iter_mut() {
        let riding = human.location == Location::Vehicle(id);
        if riding {
            move_outside(human, position, resources);
        }
        let mut lost_truck = false;
        if let Some(work) = &mut human.work {
            if matches!(work.kind, WorkKind::Driver { truck, .. } if truck == id) {
                former_drivers.push(human_id);
                work.kind = WorkKind::Worker;
                lost_truck = true;
            }
        }

        if human.router.forget_vehicle(id)
            || riding
            || lost_truck
            || human.decision.kind.references_vehicle(id)
        {
            cancel_trip(human, resources);
        }
    }
    for company in world.companies.values_mut() {
        company.comp.trucks.retain(|&truck| truck != id);
        if company
            .comp
            .driver
            .is_some_and(|driver| former_drivers.contains(&driver))
        {
            company.comp.driver = None;
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

fn clear_soul_references(sim: &mut Simulation, soul: SoulID) {
    let (world, resources) = sim.world_res();
    resources.write::<Market>().remove(soul);
    resources.write::<BuildingInfos>().remove_soul(soul);
    let bread = ItemID::new("bread");
    for human in world.humans.values_mut() {
        human.bought.0.retain(|&item, trades| {
            let count = trades.len();
            trades.retain(|trade| !trade.involves(soul));
            if item == bread && count != 0 && trades.is_empty() {
                human.food.cancel_purchase();
            }
            !trades.is_empty() || count == 0
        });
    }
    for company in world.companies.values_mut() {
        company.sold.0.retain(|trade| !trade.involves(soul));
        for trades in company.bought.0.values_mut() {
            trades.retain(|trade| !trade.involves(soul));
        }
    }
}

fn clear_building_references(sim: &mut Simulation, id: BuildingID, door: Option<Vec3>) {
    let (world, resources) = sim.world_res();
    for (human_id, human) in world.humans.iter_mut() {
        let mut cancel =
            human.router.references_building(id) || human.decision.kind.references_building(id);
        human.food.forget_building(id);

        if let Some(work) = &mut human.work {
            if work.workplace == id {
                human.work = None;
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
            } = &mut work.kind
            {
                if *deliver_order == Some(id) {
                    *deliver_order = None;
                    resources
                        .write::<Dispatcher>()
                        .free(DispatchID::SmallTruck(*truck));
                    if let Some(vehicle) = world.vehicles.get_mut(*truck) {
                        vehicle.it = Itinerary::NONE;
                    }
                    cancel = true;
                }
            }
        }

        if let Some(door) = door.filter(|_| human.location == Location::Building(id)) {
            move_outside(human, door, resources);
            cancel = true;
        }
        if cancel {
            if let Location::Vehicle(vehicle) = human.location {
                if let Some(vehicle) = world.vehicles.get_mut(vehicle) {
                    vehicle.it = Itinerary::NONE;
                }
            }
            cancel_trip(human, resources);
        }
    }
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

fn cancel_trip(human: &mut HumanEnt, resources: &Resources) {
    human
        .router
        .cancel_route(&mut resources.write::<ParkingManagement>());
    human.it = Itinerary::NONE;
    human.decision = Default::default();
}

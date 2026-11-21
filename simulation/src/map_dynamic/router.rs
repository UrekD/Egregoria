use crate::map::{BuildingID, Map, PathKind};
use crate::map_dynamic::{
    BuildingInfos, Itinerary, ParkingManagement, ParkingReserveError, SpotReservation,
    NEAR_PARKING_RADIUS,
};
use crate::souls::desire::WorkKind;
use crate::transportation::TransportGrid;
use crate::transportation::{put_pedestrian_in_transport_grid, unpark, Location};
use crate::utils::resources::Resources;
use crate::world::{HumanEnt, HumanID, VehicleEnt, VehicleID};
use crate::{ParCommandBuffer, SoulID, World};
use egui_inspect::Inspect;
use geom::{Transform, Vec3};
use serde::{Deserialize, Serialize};
use slotmapd::HopSlotMap;

#[derive(Inspect, Serialize, Deserialize)]
pub struct Router {
    steps: Vec<RoutingStep>,
    cur_step: Option<RoutingStep>,
    pub target_dest: Option<Destination>,
    cur_dest: Option<Destination>,
    vehicle: Option<VehicleID>,
    pub personal_car: Option<VehicleID>,
    pub last_error: Option<RouterError>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum RouterError {
    ReservingParkingSpot(ParkingReserveError),
    TranslatingParkingSpotToDrivePos,
    LocatingVehicle,
}

debug_inspect_impl!(RouterError);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Destination {
    Outside(Vec3),
    Building(BuildingID),
}

debug_inspect_impl!(Destination);

#[derive(Debug, Serialize, Deserialize)]
pub enum RoutingStep {
    WalkTo(Vec3),
    DriveTo(VehicleID, Vec3),
    Park(VehicleID, Option<SpotReservation>),
    Unpark(VehicleID),
    GetInVehicle(VehicleID),
    GetOutVehicle(VehicleID),
    GetInBuilding(BuildingID),
    GetOutBuilding(BuildingID),
}

debug_inspect_impl!(RoutingStep);

pub fn routing_changed_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::routing_changed_system");
    let map: &Map = &resources.read();
    let parking: &mut ParkingManagement = &mut resources.write();
    let cbuf_human = resources.read::<ParCommandBuffer<HumanEnt>>();
    let cbuf_vehicle = resources.read::<ParCommandBuffer<VehicleEnt>>();

    world.humans.iter_mut().for_each(|(id, h)| {
        let router = &mut h.router;
        if router.cur_dest == router.target_dest {
            return;
        }
        let dest = unwrap_ret!(router.target_dest);

        router.clear_steps(parking);
        let (pos, building) = match dest {
            Destination::Outside(pos) => (pos, None),
            Destination::Building(build) => {
                if h.location == Location::Building(build) {
                    router.cur_dest = router.target_dest;
                    router.last_error = None;
                    return;
                }

                let Some(building) = map.get(build) else {
                    router.cur_dest = router.target_dest;
                    return;
                };
                (building.door_pos, Some(build))
            }
        };

        let mut route = router.steps_to(pos, parking, map, &h.location, &world.vehicles);
        if matches!(route, Err(RouterError::ReservingParkingSpot(_))) {
            if let Some(car) = router
                .personal_car
                .filter(|car| router.vehicle == Some(*car))
            {
                let truck = h.work.as_ref().is_some_and(
                    |work| matches!(work.kind, WorkKind::Driver { truck, .. } if truck == car),
                );
                if !truck {
                    if h.location == Location::Vehicle(car) {
                        router.forget_vehicle(car);
                        let exit_pos = world.vehicles.get(car).map_or(h.trans.pos, |v| v.trans.pos);
                        walk_outside(id, exit_pos, &cbuf_human, &mut h.location);
                        h.it = Itinerary::NONE;
                        cbuf_vehicle.kill(car);
                    }
                    let vehicle = router.vehicle.take();
                    route = router.steps_to(pos, parking, map, &h.location, &world.vehicles);
                    router.vehicle = vehicle;
                }
            }
        }

        router.steps = match route {
            Ok(steps) => steps,
            Err(error) => {
                router.last_error = Some(error);
                return;
            }
        };
        if let Some(building) = building {
            router.steps.push(RoutingStep::GetInBuilding(building));
        }

        router.last_error = None;
        router.cur_dest = router.target_dest;

        router.steps.reverse();
    });
}

pub fn routing_update_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::routing_update_system");
    let map: &Map = &resources.read();
    let cbuf_human: &ParCommandBuffer<HumanEnt> = &resources.read();
    let cbuf_vehicle: &ParCommandBuffer<VehicleEnt> = &resources.read();

    world.humans.iter_mut().for_each(|(body, h)| {
        if h.router.cur_step.is_none() && h.router.steps.is_empty() {
            return;
        }

        let trans: &Transform = &h.trans;
        let itin: &Itinerary = &h.it;

        let pos = match h.location {
            Location::Outside => trans.pos,
            Location::Vehicle(id) => world
                .vehicles
                .get(id)
                .map(|x| x.trans.pos)
                .unwrap_or_else(|| trans.pos),
            Location::Building(id) => map
                .buildings()
                .get(id)
                .map(|b| b.door_pos)
                .unwrap_or_else(|| trans.pos),
        };

        let mut cur_step_over = true;

        if let Some(ref step) = h.router.cur_step {
            cur_step_over = match *step {
                RoutingStep::WalkTo(_) => itin.has_ended(0.0),
                RoutingStep::DriveTo(vehicle, _) => world
                    .vehicles
                    .get(vehicle)
                    .map(|x| &x.it)
                    .map(|x| x.has_ended(0.0))
                    .unwrap_or(true),
                RoutingStep::Park(vehicle, _) => world
                    .vehicles
                    .get(vehicle)
                    .map(|x| &x.vehicle)
                    .map(|x| x.state.is_parked())
                    .unwrap_or(true),
                RoutingStep::Unpark(_) => true,
                RoutingStep::GetInVehicle(_) => true,
                RoutingStep::GetOutVehicle(_) => true,
                RoutingStep::GetInBuilding(_) => true,
                RoutingStep::GetOutBuilding(_) => true,
            };
        }
        let mut next_step_ready = true;

        if let Some(step) = h.router.steps.last() {
            next_step_ready = match *step {
                RoutingStep::WalkTo(_) => true,
                RoutingStep::DriveTo(_, _) => true,
                RoutingStep::Park(_, _) => true,
                RoutingStep::Unpark(_) => true,
                RoutingStep::GetInVehicle(vehicle) => world
                    .vehicles
                    .get(vehicle)
                    .map(|v| v.trans.pos.is_close(pos, 3.0))
                    .unwrap_or(true),
                RoutingStep::GetOutVehicle(_) => true,
                RoutingStep::GetInBuilding(build) => map
                    .buildings()
                    .get(build)
                    .map(|b| b.door_pos.is_close(pos, 3.0))
                    .unwrap_or(true),
                RoutingStep::GetOutBuilding(_) => true,
            };
        }

        if !(next_step_ready && cur_step_over) {
            return;
        }

        h.router.cur_step = h.router.steps.pop();

        if let Some(ref mut next_step) = h.router.cur_step {
            match *next_step {
                RoutingStep::WalkTo(obj) => {
                    h.it = Itinerary::wait_for_reroute(PathKind::Pedestrian, obj);
                }
                RoutingStep::DriveTo(vehicle, obj) => {
                    let route = Itinerary::wait_for_reroute(PathKind::Vehicle, obj);
                    if let Some(x) = world.vehicles.get_mut(vehicle) {
                        x.it = route
                    }
                }
                RoutingStep::Park(vehicle, ref mut spot) => {
                    if let Some(spot_resa) = spot.take() {
                        let Some(spot) = spot_resa.get(&map.parking) else {
                            h.router.reset_dest();
                            return;
                        };

                        if let Some(vehicle) = world.vehicles.get_mut(vehicle) {
                            vehicle.begin_parking(spot.trans, spot_resa);
                        }
                    }
                }
                RoutingStep::Unpark(vehicle) => {
                    cbuf_vehicle.exec_ent(vehicle, move |sim| unpark(sim, vehicle));
                }
                RoutingStep::GetInVehicle(vehicle) => {
                    if !world.vehicles.contains_key(vehicle) {
                        h.router.reset_dest();
                        return;
                    }
                    h.location = Location::Vehicle(vehicle);
                    walk_inside(body, h, cbuf_human);
                }
                RoutingStep::GetOutVehicle(vehicle) => {
                    let pos = world
                        .vehicles
                        .get(vehicle)
                        .map(|v| v.trans)
                        .map(|vtrans| vtrans.pos + vtrans.dir.cross(Vec3::Z) * 2.0)
                        .unwrap_or(pos);
                    walk_outside(body, pos, cbuf_human, &mut h.location);
                }
                RoutingStep::GetInBuilding(build) => {
                    if !map.buildings().contains_key(build) {
                        h.router.reset_dest();
                        return;
                    }
                    h.location = Location::Building(build);
                    resources
                        .write::<BuildingInfos>()
                        .get_in(build, SoulID::Human(body));
                    walk_inside(body, h, cbuf_human);
                }
                RoutingStep::GetOutBuilding(build) => {
                    let wpos = map
                        .buildings()
                        .get(build)
                        .map(|x| x.door_pos)
                        .unwrap_or(pos);
                    resources
                        .write::<BuildingInfos>()
                        .get_out(build, SoulID::Human(body));
                    walk_outside(body, wpos, cbuf_human, &mut h.location);
                }
            }
        }
    })
}

fn walk_inside(body: HumanID, h: &mut HumanEnt, cbuf: &ParCommandBuffer<HumanEnt>) {
    if let Some(coll) = h.collider.take() {
        cbuf.exec_ent(body, coll.destroy());
    }
    h.speed.0 = 0.0;
}

fn walk_outside(body: HumanID, pos: Vec3, cbuf: &ParCommandBuffer<HumanEnt>, loc: &mut Location) {
    *loc = Location::Outside;
    cbuf.exec_ent(body, move |sim| {
        let coll = put_pedestrian_in_transport_grid(&mut sim.write::<TransportGrid>(), pos);
        let h = unwrap_ret!(sim.world.humans.get_mut(body));
        h.trans.pos = pos;
        h.collider = Some(coll);
    });
}

impl Router {
    pub fn new(personal_car: Option<VehicleID>) -> Self {
        Self {
            steps: vec![],
            cur_step: None,
            target_dest: None,
            personal_car,
            vehicle: personal_car,
            cur_dest: None,
            last_error: None,
        }
    }

    pub fn use_vehicle(&mut self, v: Option<VehicleID>) {
        self.vehicle = v;
    }

    pub(crate) fn selected_vehicle(&self) -> Option<VehicleID> {
        self.vehicle
    }

    pub(crate) fn clear_steps(&mut self, parking: &mut ParkingManagement) {
        for s in self.steps.drain(..).chain(self.cur_step.take()) {
            if let RoutingStep::Park(_, Some(spot)) = s {
                parking.free(spot);
            }
        }
    }

    pub(crate) fn references_building(&self, building: BuildingID) -> bool {
        self.target_dest == Some(Destination::Building(building))
            || self.cur_dest == Some(Destination::Building(building))
            || self.steps.iter().chain(self.cur_step.iter()).any(|step| {
                matches!(step, RoutingStep::GetInBuilding(id) | RoutingStep::GetOutBuilding(id) if *id == building)
            })
    }

    pub(crate) fn forget_vehicle(&mut self, id: VehicleID) -> bool {
        if self.personal_car == Some(id) {
            self.personal_car = None;
        }
        let selected = self.vehicle == Some(id);
        if selected {
            self.vehicle = self.personal_car;
        }
        selected
            || self
                .steps
                .iter()
                .chain(self.cur_step.iter())
                .any(|step| match *step {
                    RoutingStep::DriveTo(vehicle, _)
                    | RoutingStep::Park(vehicle, _)
                    | RoutingStep::Unpark(vehicle)
                    | RoutingStep::GetInVehicle(vehicle)
                    | RoutingStep::GetOutVehicle(vehicle) => vehicle == id,
                    _ => false,
                })
    }

    pub(crate) fn cancel_route(&mut self, parking: &mut ParkingManagement) {
        self.clear_steps(parking);
        self.cur_dest = None;
        self.target_dest = None;
        self.last_error = None;
    }

    pub fn reset_dest(&mut self) {
        self.cur_dest = None;
    }

    /// Returns wheter or not the destination was already attained
    pub fn go_to(&mut self, dest: Destination) -> bool {
        if let Some(router_dest) = self.cur_dest {
            if router_dest == dest {
                return self.steps.is_empty() && self.cur_step.is_none();
            }
        }
        self.target_dest = Some(dest);
        false
    }

    fn steps_to(
        &mut self,
        obj: Vec3,
        parking: &mut ParkingManagement,
        map: &Map,
        loc: &Location,
        cars: &HopSlotMap<VehicleID, VehicleEnt>,
    ) -> Result<Vec<RoutingStep>, RouterError> {
        let mut steps = vec![];
        if let Location::Building(cur_build) = loc {
            steps.push(RoutingStep::GetOutBuilding(*cur_build));
        }

        if let Location::Vehicle(current) = *loc {
            if self.vehicle != Some(current) {
                steps.push(RoutingStep::GetOutVehicle(current));
            }
        }

        if let Some(car) = self.vehicle {
            let spot_resa = if self.personal_car == Some(car) {
                parking
                    .reserve_near_radius(obj, map, NEAR_PARKING_RADIUS)
                    .or_else(|_| parking.reserve_near_radius(obj, map, 4.0 * NEAR_PARKING_RADIUS))
            } else {
                parking.reserve_near(obj, map)
            }
            .map_err(RouterError::ReservingParkingSpot)?;
            let parking_pos = match spot_resa.park_pos(map) {
                Some(x) => x,
                None => {
                    parking.free(spot_resa);
                    return Err(RouterError::TranslatingParkingSpotToDrivePos);
                }
            };

            if *loc != Location::Vehicle(car) {
                if let Some(pos) = cars.get(car).map(|x| x.trans.pos) {
                    steps.push(RoutingStep::WalkTo(pos));
                    steps.push(RoutingStep::GetInVehicle(car));
                    steps.push(RoutingStep::Unpark(car));
                } else {
                    parking.free(spot_resa);
                    self.vehicle = None;
                    return Err(RouterError::LocatingVehicle);
                }
            }

            steps.push(RoutingStep::DriveTo(car, parking_pos));
            steps.push(RoutingStep::Park(car, Some(spot_resa)));
            steps.push(RoutingStep::GetOutVehicle(car));
        }

        steps.push(RoutingStep::WalkTo(obj));
        Ok(steps)
    }
}

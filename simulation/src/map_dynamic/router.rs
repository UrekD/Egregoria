use crate::economy::{on_arrival, on_cancel, on_start, TripMode, TripProgress};
use crate::map::{BuildingID, Map, PathKind};
use crate::map_dynamic::transit::{elapsed_ticks, WAIT_NOT_STARTED};
use crate::map_dynamic::{
    BuildingInfos, Itinerary, LineID, ParkingManagement, ParkingReserveError, SpotReservation,
    StopID, Transit, NEAR_PARKING_RADIUS,
};
use crate::souls::desire::WorkKind;
use crate::transportation::TransportGrid;
use crate::transportation::{put_pedestrian_in_transport_grid, unpark, Location, VehicleKind};
use crate::utils::resources::Resources;
use crate::world::{HumanEnt, HumanID, VehicleEnt, VehicleID};
use crate::{ParCommandBuffer, SoulID, World};
use egui_inspect::Inspect;
use geom::{Transform, Vec3};
use prototypes::Tick;
use serde::{Deserialize, Serialize};
use slotmapd::HopSlotMap;

#[path = "transit_path.rs"]
mod transit_path;
use transit_path::{legacy_cost, TransitPaths, ValidatedWalk};

const PERSONAL_CAR_ACCESS_RADIUS: f32 = 4.0 * NEAR_PARKING_RADIUS;

#[derive(Inspect, Serialize, Deserialize)]
pub struct Router {
    steps: Vec<RoutingStep>,
    cur_step: Option<RoutingStep>,
    pub target_dest: Option<Destination>,
    cur_dest: Option<Destination>,
    vehicle: Option<VehicleID>,
    pub personal_car: Option<VehicleID>,
    pub last_error: Option<RouterError>,
    pub trip_start: Option<Tick>,
    #[inspect(skip)]
    pub(crate) trip_mode: TripProgress,
    pub(crate) walk_only_pickup: bool,
    replan_at_alight: bool,
    #[inspect(skip)]
    #[serde(skip)]
    transit_walks: Option<Box<[Option<ValidatedWalk>; 2]>>,
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
    WaitAtStop {
        stop: StopID,
        line: LineID,
        /// Stamped in elapsed ticks by the first transit_board_update of this step.
        since: Tick,
    },
    RideTransit {
        line: LineID,
        alight: StopID,
    },
}

debug_inspect_impl!(RoutingStep);

pub fn routing_changed_system(world: &mut World, resources: &mut Resources) {
    routing_changed(world, resources, Transit::headway);
}

fn routing_changed(
    world: &mut World,
    resources: &mut Resources,
    headway: impl Fn(&Transit, LineID) -> Option<u64>,
) {
    profiling::scope!("map_dynamic::routing_changed_system");
    let map: &Map = &resources.read();
    let parking: &mut ParkingManagement = &mut resources.write();
    let cbuf_human = resources.read::<ParCommandBuffer<HumanEnt>>();
    let cbuf_vehicle = resources.read::<ParCommandBuffer<VehicleEnt>>();
    let transit = resources.read::<Transit>();
    let mut transit_paths = None;

    world.humans.iter_mut().for_each(|(id, h)| {
        let router = &mut h.router;
        if matches!(h.location, Location::Transit(_)) {
            return;
        }
        if std::mem::take(&mut router.replan_at_alight) {
            if matches!(router.cur_step, Some(RoutingStep::RideTransit { .. })) {
                router.cur_step = None;
            }
            router.reset_dest();
            match router.target_dest {
                Some(Destination::Building(build)) if map.get(build).is_none() => {
                    router.cancel_route(parking);
                    h.decision = Default::default();
                }
                None => router.clear_steps(parking),
                _ => {}
            }
        }
        if router.cur_dest == router.target_dest {
            return;
        }
        let dest = unwrap_ret!(router.target_dest);

        let replacing_wait = matches!(router.cur_step, Some(RoutingStep::WaitAtStop { .. }));
        router.clear_steps(parking);
        if replacing_wait && h.location == Location::Outside && h.collider.is_none() {
            Router::restore_walking(h, h.trans.pos, resources);
        }
        let router = &mut h.router;
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
        if router.trip_start.is_none() {
            router.trip_start = Some(elapsed_ticks(resources));
        }

        let mut transit_steps = None;
        if !transit.runs.is_empty()
            && router.allows_transit()
            && !matches!(h.location, Location::Vehicle(_))
            && (router.vehicle.is_none() || router.vehicle == router.personal_car)
            && !h
                .work
                .as_ref()
                .is_some_and(|work| matches!(work.kind, WorkKind::Driver { .. }))
        {
            let car = router.vehicle.and_then(|id| world.vehicles.get(id));
            if car.is_none_or(|vehicle| matches!(vehicle.vehicle.kind, VehicleKind::Car)) {
                let now = elapsed_ticks(resources);
                let paths =
                    transit_paths.get_or_insert_with(|| TransitPaths::new(&transit, now, &headway));
                if paths.has_service() {
                    let origin = match h.location {
                        Location::Building(id) => map.get(id).map_or(h.trans.pos, |b| b.door_pos),
                        _ => h.trans.pos,
                    };
                    let nearby_car = car.map(|car| car.trans.pos).filter(|&car_pos| {
                        car_pos.distance2(origin) <= PERSONAL_CAR_ACCESS_RADIUS.powi(2)
                    });
                    let walk_speed = f64::from(h.pedestrian.walking_speed);
                    transit_steps = paths.winning_steps(
                        origin,
                        pos,
                        &h.location,
                        map,
                        now,
                        legacy_cost(origin, pos, nearby_car, walk_speed),
                        walk_speed,
                        |line| transit.allows_line_for_trip(id, router, line),
                    );
                }
            }
        }

        let mut route = if let Some((steps, paths)) = transit_steps {
            router.transit_walks = Some(Box::new(paths.map(Some)));
            Ok(steps)
        } else {
            router.steps_to(pos, parking, map, &h.location, &world.vehicles)
        };
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
        let mode = if router
            .steps
            .iter()
            .any(|step| matches!(step, RoutingStep::RideTransit { .. }))
        {
            TripMode::Transit
        } else if router
            .steps
            .iter()
            .any(|step| matches!(step, RoutingStep::DriveTo(..)))
        {
            TripMode::Car
        } else {
            TripMode::Walk
        };
        router.trip_mode.plan(mode);
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
            Location::Vehicle(id) | Location::Transit(id) => world
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
                RoutingStep::WaitAtStop { .. } => matches!(h.location, Location::Transit(_)),
                RoutingStep::RideTransit { .. } => !matches!(h.location, Location::Transit(_)),
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
                RoutingStep::WaitAtStop { .. } => true,
                RoutingStep::RideTransit { .. } => matches!(h.location, Location::Transit(_)),
            };
        }

        if !(next_step_ready && cur_step_over) {
            return;
        }

        h.router.cur_step = h.router.steps.pop();

        let transit_walk = matches!(h.router.steps.last(), Some(RoutingStep::WaitAtStop { .. }))
            || matches!(h.router.trip_mode, TripProgress::RodeTransit);
        if let Some(ref mut next_step) = h.router.cur_step {
            match *next_step {
                RoutingStep::WalkTo(obj) => {
                    let mut route = None;
                    if transit_walk {
                        // Rebuild the same itinerary after loading or invalidating a cached walk.
                        route = h
                            .router
                            .transit_walks
                            .as_deref_mut()
                            .and_then(|paths| {
                                paths.iter_mut().find_map(|path| {
                                    if path
                                        .as_ref()
                                        .is_some_and(|path| path.matches(pos, obj, map))
                                    {
                                        path.take().map(ValidatedWalk::into_itinerary)
                                    } else {
                                        None
                                    }
                                })
                            })
                            .or_else(|| {
                                Itinerary::route(
                                    elapsed_ticks(resources),
                                    pos,
                                    obj,
                                    map,
                                    PathKind::Pedestrian,
                                )
                            });
                    }
                    h.it = route
                        .unwrap_or_else(|| Itinerary::wait_for_reroute(PathKind::Pedestrian, obj));
                }
                RoutingStep::DriveTo(vehicle, obj) => {
                    let route = Itinerary::wait_for_reroute(PathKind::Vehicle, obj);
                    if let Some(x) = world.vehicles.get_mut(vehicle) {
                        x.it = route;
                        if h.location == Location::Vehicle(vehicle) {
                            h.router.trip_mode.drove();
                        }
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
                    on_arrival(&mut h.router, resources);
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
                RoutingStep::WaitAtStop { ref mut since, .. } => *since = WAIT_NOT_STARTED,
                RoutingStep::RideTransit { .. } => {}
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
            trip_start: None,
            trip_mode: TripProgress::Walk,
            walk_only_pickup: false,
            replan_at_alight: false,
            transit_walks: None,
        }
    }

    pub fn cur_step(&self) -> Option<&RoutingStep> {
        self.cur_step.as_ref()
    }

    pub(crate) fn allows_transit(&self) -> bool {
        !self.walk_only_pickup
    }

    pub(crate) fn stamp_wait_start(&mut self, now: Tick) {
        if let Some(RoutingStep::WaitAtStop { since, .. }) = &mut self.cur_step {
            if *since == WAIT_NOT_STARTED {
                *since = now;
            }
        }
    }

    /// Start the next ride after the boarding system has checked the bus and stop.
    pub(crate) fn board(human: &mut HumanEnt, bus: VehicleID, resources: &Resources) -> bool {
        let router = &mut human.router;
        if human.location != Location::Outside || !router.allows_transit() {
            return false;
        }
        let Some(RoutingStep::WaitAtStop { line, .. }) = router.cur_step else {
            return false;
        };
        if !matches!(router.steps.last(), Some(RoutingStep::RideTransit { line: next, .. }) if *next == line)
        {
            return false;
        }

        router.cur_step = router.steps.pop();
        human.location = Location::Transit(bus);
        human.it = Itinerary::NONE;
        human.speed.0 = 0.0;
        if let Some(collider) = human.collider.take() {
            resources
                .write::<TransportGrid>()
                .remove_maintain(collider.0);
        }
        true
    }

    /// End a ride at a walkable stop position, keeping the remaining route.
    pub(crate) fn alight(human: &mut HumanEnt, stop_pos: Vec3, resources: &Resources) {
        if !matches!(human.location, Location::Transit(_)) {
            return;
        }
        human.router.cur_step = None;
        Self::restore_walking(human, stop_pos, resources);
    }

    /// Leave transit at a walkable position and plan again from there.
    pub(crate) fn force_detach(human: &mut HumanEnt, pos: Vec3, resources: &Resources) {
        if !matches!(human.location, Location::Transit(_))
            && !matches!(human.router.cur_step, Some(RoutingStep::WaitAtStop { .. }))
        {
            return;
        }
        human.router.cur_step = None;
        human.router.defer_replan();
        Self::restore_walking(human, pos, resources);
    }

    pub(crate) fn defer_replan(&mut self) {
        self.replan_at_alight = true;
    }

    fn restore_walking(human: &mut HumanEnt, pos: Vec3, resources: &Resources) {
        let mut grid = resources.write::<TransportGrid>();
        if let Some(collider) = human.collider.take() {
            grid.remove_maintain(collider.0);
        }
        let collider = put_pedestrian_in_transport_grid(&mut grid, pos);
        grid.get_mut(collider.0).unwrap().1.height = pos.z;
        human.collider = Some(collider);
        human.location = Location::Outside;
        human.trans.pos = pos;
        human.it = Itinerary::NONE;
        human.speed.0 = 0.0;
    }

    #[cfg(test)]
    pub(crate) fn set_test_route(&mut self, dest: Destination, steps: Vec<RoutingStep>) {
        self.target_dest = Some(dest);
        self.cur_dest = Some(dest);
        self.steps = steps.into_iter().rev().collect();
        self.cur_step = self.steps.pop();
    }

    pub fn references_stop(&self, stop: StopID) -> bool {
        self.steps.iter().chain(self.cur_step()).any(|step| match *step {
            RoutingStep::WaitAtStop { stop: id, .. } | RoutingStep::RideTransit { alight: id, .. } => {
                id == stop
            }
            _ => false,
        })
    }

    pub fn use_vehicle(&mut self, v: Option<VehicleID>) {
        self.vehicle = v;
    }

    pub(crate) fn selected_vehicle(&self) -> Option<VehicleID> {
        self.vehicle
    }

    pub(crate) fn clear_steps(&mut self, parking: &mut ParkingManagement) {
        self.transit_walks = None;
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
        if matches!(self.cur_step, Some(RoutingStep::RideTransit { .. })) {
            self.defer_replan();
            return;
        }
        on_cancel(self);
        self.clear_steps(parking);
        self.cur_dest = None;
        self.target_dest = None;
        self.last_error = None;
        self.replan_at_alight = false;
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
        if self.target_dest != Some(dest) {
            self.target_dest = Some(dest);
            on_start(self);
        }
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
                    .or_else(|_| parking.reserve_near_radius(obj, map, PERSONAL_CAR_ACCESS_RADIUS))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::LaneKind;
    use crate::map_dynamic::{BusRun, TransitLine, TransitStop};
    use crate::souls::desire::Work;
    use crate::souls::human::spawn_human;
    use crate::tests::TestCtx;
    use crate::transportation::{spawn_driving_vehicle, VehicleKind};
    use crate::world_command::WorldCommand;
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Color};
    use prototypes::Money;

    struct RouteFixture {
        ctx: TestCtx,
        human: HumanID,
        line: LineID,
        stops: [StopID; 2],
        origin: Vec3,
        destination: Vec3,
    }

    impl RouteFixture {
        fn new(distance: f32) -> Self {
            let mut ctx = TestCtx::new();
            ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(900.0, 100.0, 0.0)]);
            let house = ctx.build_house_near(vec2(200.0, 100.0));
            let human = spawn_human(&mut ctx.g, house).unwrap();
            let origin = Self::sidewalk_at(&ctx, vec3(200.0, 100.0, 0.0));
            let destination = origin + Vec3::X * distance;
            let berths = [origin, destination].map(|pos| {
                let map = ctx.g.map();
                map.get(map.nearest_lane(pos, LaneKind::Driving, None).unwrap())
                    .unwrap()
                    .points
                    .project_segment_dir(pos)
            });
            let (line, stops) = {
                let mut transit = ctx.g.write::<Transit>();
                let stops = [(origin, berths[0]), (destination, berths[1])].map(
                    |(walk_pos, (berth, _, dir))| {
                        transit.stops.insert_with_key(|id| TransitStop {
                            id,
                            pos: berth,
                            dir: dir.xy(),
                            walk_pos,
                            berth,
                        })
                    },
                );
                let line = transit.lines.insert_with_key(|id| TransitLine {
                    id,
                    stops: stops.to_vec(),
                    target_vehicles: 1,
                    fare: Money::ZERO,
                    color: Color::WHITE,
                    suspended: false,
                });
                (line, stops)
            };
            let (berth, _, dir) = berths[0];
            let bus =
                spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, Transform { pos: berth, dir })
                    .unwrap();
            ctx.g.write::<Transit>().runs.insert(
                bus,
                BusRun {
                    line,
                    next_stop_idx: 1,
                    dwell_until: None,
                    leg_started: Tick(0),
                    failures: 0,
                },
            );

            let traveler = &mut ctx.g.world.humans[human];
            traveler.location = Location::Outside;
            traveler.trans.pos = origin;
            traveler.router = Router::new(None);
            traveler.pedestrian.walking_speed = 1.2;
            Self {
                ctx,
                human,
                line,
                stops,
                origin,
                destination,
            }
        }

        fn sidewalk_at(ctx: &TestCtx, pos: Vec3) -> Vec3 {
            let map = ctx.g.map();
            map.get(map.nearest_lane(pos, LaneKind::Walking, None).unwrap())
                .unwrap()
                .points
                .project(pos)
        }

        fn assign_car(&mut self, position: Vec3) -> VehicleID {
            let car =
                spawn_driving_vehicle(&mut self.ctx.g, VehicleKind::Car, Transform::new(position))
                    .unwrap();
            self.ctx.g.world.humans[self.human].router = Router::new(Some(car));
            car
        }

        // Analytical cost and fallback cases use a fixed service interval.
        fn plan(&mut self, headway: u64) -> &[RoutingStep] {
            let (world, resources) = self.ctx.g.world_res();
            world.humans[self.human]
                .router
                .go_to(Destination::Outside(self.destination));
            routing_changed(world, resources, |_, line| {
                (line == self.line).then_some(headway)
            });
            &world.humans[self.human].router.steps
        }

        fn assert_ride(&mut self, headway: u64) {
            let [board, alight] = self.stops;
            let line = self.line;
            let origin = self.origin;
            let destination = self.destination;
            assert!(matches!(
                self.plan(headway),
                [RoutingStep::WalkTo(end), RoutingStep::RideTransit { line: ride, alight: off },
                    RoutingStep::WaitAtStop { stop: on, line: wait, since }, RoutingStep::WalkTo(start)]
                    if *end == destination && *start == origin
                        && *ride == line && *wait == line && *on == board && *off == alight
                        && *since == WAIT_NOT_STARTED
            ));
        }
    }

    #[test]
    fn p3b_zero_running_buses_preserve_legacy_steps() {
        let mut f = RouteFixture::new(500.0);
        let car = f.assign_car(f.origin + Vec3::X * 350.0);
        f.ctx.g.write::<Transit>().runs.clear();
        let mut expected = {
            let map = f.ctx.g.map();
            let parking = Bincode::encode(&*f.ctx.g.read::<ParkingManagement>()).unwrap();
            Router::new(Some(car))
                .steps_to(
                    f.destination,
                    &mut Bincode::decode(&parking).unwrap(),
                    &map,
                    &Location::Outside,
                    &f.ctx.g.world.vehicles,
                )
                .unwrap()
        };
        assert!(expected
            .iter()
            .any(|step| matches!(step, RoutingStep::DriveTo(..))));
        expected.reverse();
        assert_eq!(
            Bincode::encode(&f.plan(400)).unwrap(),
            Bincode::encode(&expected).unwrap(),
        );
    }

    #[test]
    fn p3b_carless_commuter_near_stops_chooses_transit() {
        RouteFixture::new(500.0).assert_ride(400);
    }

    #[test]
    fn p3b_owner_far_from_car_chooses_transit_without_reserving_parking() {
        let mut f = RouteFixture::new(500.0);
        let car = f.assign_car(f.origin + Vec3::X * 401.0);
        let before = Bincode::encode(&*f.ctx.g.read::<ParkingManagement>()).unwrap();
        f.assert_ride(400);
        let after = Bincode::encode(&*f.ctx.g.read::<ParkingManagement>()).unwrap();
        assert_eq!(after, before);
        assert_eq!(
            f.ctx.g.world.humans[f.human].router.selected_vehicle(),
            Some(car)
        );
    }

    #[test]
    fn p3b_pickup_stays_walk_only() {
        let mut f = RouteFixture::new(500.0);
        f.ctx.g.world.humans[f.human].router.walk_only_pickup = true;
        let destination = f.destination;
        assert!(matches!(f.plan(400), [RoutingStep::WalkTo(end)] if *end == destination));
    }

    #[test]
    fn p3b_driver_truck_and_in_vehicle_trips_keep_legacy_routes() {
        let mut driver = RouteFixture::new(500.0);
        let human = &mut driver.ctx.g.world.humans[driver.human];
        human.work = Some(Work::new(
            human.home.house,
            WorkKind::Driver {
                deliver_order: None,
                truck: VehicleID::default(),
            },
            0.0,
        ));
        assert!(matches!(driver.plan(400), [RoutingStep::WalkTo(_)]));

        let mut truck = RouteFixture::new(500.0);
        let vehicle = truck.assign_car(truck.origin + Vec3::X * 50.0);
        truck.ctx.g.world.vehicles[vehicle].vehicle.kind = VehicleKind::Truck;
        assert!(truck
            .plan(400)
            .iter()
            .any(|step| matches!(step, RoutingStep::DriveTo(..))));

        let mut occupied = RouteFixture::new(500.0);
        let vehicle = occupied.assign_car(occupied.origin + Vec3::X * 50.0);
        occupied.ctx.g.world.humans[occupied.human].location = Location::Vehicle(vehicle);
        assert!(occupied
            .plan(400)
            .iter()
            .any(|step| matches!(step, RoutingStep::DriveTo(..))));
    }

    #[test]
    fn p3b_replacing_a_wait_restores_the_walking_collider() {
        let mut f = RouteFixture::new(500.0);
        let human = &mut f.ctx.g.world.humans[f.human];
        human.router.set_test_route(
            Destination::Outside(f.destination),
            vec![RoutingStep::WaitAtStop {
                stop: f.stops[0],
                line: f.line,
                since: Tick(0),
            }],
        );
        let collider = human.collider.take();
        if let Some(collider) = collider {
            f.ctx.g.write::<TransportGrid>().remove_maintain(collider.0);
        }
        f.destination = f.origin + Vec3::X * 20.0;
        assert!(matches!(f.plan(400), [RoutingStep::WalkTo(_)]));
        assert!(f.ctx.g.world.humans[f.human].collider.is_some());
    }

    #[test]
    fn p3b_unreachable_access_or_egress_falls_back_to_legacy() {
        for stop_index in 0..2 {
            let mut f = RouteFixture::new(500.0);
            f.ctx
                .build_roads(&[vec3(100.0, 200.0, 0.0), vec3(900.0, 200.0, 0.0)]);
            let endpoint = [f.origin, f.destination][stop_index];
            let disconnected = RouteFixture::sidewalk_at(&f.ctx, endpoint + Vec3::Y * 100.0);
            let result = Itinerary::route(
                Tick(0),
                endpoint,
                disconnected,
                &f.ctx.g.map(),
                PathKind::Pedestrian,
            );
            assert!(result.is_none(), "fixture must have disconnected sidewalks");
            f.ctx.g.write::<Transit>().stops[f.stops[stop_index]].walk_pos = disconnected;
            let destination = f.destination;
            assert!(matches!(f.plan(400), [RoutingStep::WalkTo(end)] if *end == destination));
        }
    }

    #[test]
    fn p3b_ties_prefer_legacy() {
        let mut tie = RouteFixture::new(240.0);
        let destination = tie.destination;
        // Walking is 10,000 ticks. Riding takes 1,500 ticks plus half the headway.
        assert!(matches!(tie.plan(17_000), [RoutingStep::WalkTo(end)] if *end == destination));
        RouteFixture::new(240.0).assert_ride(16_999);
    }

    #[test]
    fn p3b_fractional_tick_tie_prefers_legacy() {
        let mut f = RouteFixture::new(454.0);
        for (stop, offset) in f.stops.into_iter().zip([41.0, 413.0]) {
            let mut transit = f.ctx.g.write::<Transit>();
            transit.stops[stop].walk_pos = f.origin + Vec3::X * offset;
            transit.stops[stop].berth = f.origin + Vec3::X * offset;
        }
        let destination = f.destination;
        // Both estimates are exactly 56,750 / 3 ticks.
        assert!(matches!(f.plan(26_350), [RoutingStep::WalkTo(end)] if *end == destination));
    }

    #[test]
    fn p3b_active_service_without_a_winning_ride_preserves_parked_car() {
        let mut f = RouteFixture::new(500.0);
        let car = f.assign_car(f.origin + Vec3::X * 350.0);
        {
            let mut transit = f.ctx.g.write::<Transit>();
            for stop in transit.stops.values_mut() {
                stop.walk_pos += Vec3::Y * 700.0;
            }
        }
        assert!(f
            .plan(400)
            .iter()
            .any(|step| { matches!(step, RoutingStep::DriveTo(vehicle, _) if *vehicle == car) }));
        assert_eq!(
            f.ctx.g.world.humans[f.human].router.trip_mode.mode(),
            TripMode::Car
        );
    }

    #[test]
    fn p3b_ride_cost_follows_the_order_around_the_loop() {
        let mut f = RouteFixture::new(240.0);
        let [alight, board] = f.stops;
        {
            let mut transit = f.ctx.g.write::<Transit>();
            let far = f.origin + Vec3::Y * 1_000.0;
            let via = transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos: far,
                walk_pos: far,
                berth: far,
                dir: vec2(1.0, 0.0),
            });
            transit.lines[f.line].stops = vec![alight, board, via];
        }
        f.destination = f.origin;
        f.ctx.g.world.humans[f.human].trans.pos = f.origin + Vec3::X * 240.0;
        let destination = f.destination;
        assert!(matches!(f.plan(400), [RoutingStep::WalkTo(end)] if *end == destination));
    }

    #[test]
    fn p3b_carless_commuter_uses_scheduled_headway() {
        let mut f = RouteFixture::new(500.0);
        let old_bus = *f.ctx.g.read::<Transit>().runs.keys().next().unwrap();
        let old_vehicle = f.ctx.g.world.vehicles.remove(old_bus).unwrap();
        f.ctx
            .g
            .write::<TransportGrid>()
            .remove_maintain(old_vehicle.collider.unwrap().0);
        *f.ctx.g.write::<Transit>() = Transit::default();
        let commands = {
            let map = f.ctx.g.map();
            let lane = map
                .lanes()
                .values()
                .find(|lane| {
                    lane.kind == LaneKind::Driving && lane.points.first().x < lane.points.last().x
                })
                .unwrap();
            [f.origin, f.destination].map(|pos| {
                let (pos, _, dir) = lane.points.project_segment_dir(pos);
                WorldCommand::TransitAddStop { pos, dir: dir.xy() }
            })
        };
        f.ctx.apply(&commands);
        let stops: Vec<_> = f.ctx.g.read::<Transit>().stops.keys().collect();
        assert_eq!(stops.len(), 2);
        f.ctx.apply(&[WorldCommand::TransitCreateLine { stops }]);
        let line = f.ctx.g.read::<Transit>().lines.keys().next().unwrap();
        f.ctx
            .apply(&[WorldCommand::TransitSetVehicles { line, n: 1 }]);
        f.ctx.tick_fast();
        {
            let transit = f.ctx.g.read::<Transit>();
            let (legs, dwell) = transit.directed_leg_times(line).unwrap();
            assert_eq!(
                transit.headway(line),
                Some(legs.iter().sum::<u64>() + legs.len() as u64 * dwell)
            );
        }

        let (world, resources) = f.ctx.g.world_res();
        let human = &mut world.humans[f.human];
        human.router.go_to(Destination::Outside(f.destination));
        routing_changed_system(world, resources);
        let router = &world.humans[f.human].router;
        assert!(router
            .steps
            .iter()
            .any(|step| matches!(step, RoutingStep::RideTransit { line: id, .. } if *id == line)));
        assert_eq!(router.trip_mode.mode(), TripMode::Transit);
    }
}

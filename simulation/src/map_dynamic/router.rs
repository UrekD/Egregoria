use crate::economy::{on_arrival, on_cancel, on_start, TripMode, TripProgress, TripPurpose};
use crate::map::building_parking::BuildingParking;
use crate::map::{BuildingID, Map, PathKind, Pathfinder, TraverseDirection};
use crate::map_dynamic::transit::{elapsed_ticks, WAIT_NOT_STARTED};
use crate::map_dynamic::{
    BuildingInfos, DirectedEndpoint, DirectedPlan, DirectedSearch, DirectedSearchResult, Itinerary, LineID, ParkingManagement, ParkingReserveError, SpotReservation,
    StopID, Transit, NEAR_PARKING_RADIUS, OBJECTIVE_OK_DIST,
};
use crate::souls::desire::WorkKind;
use crate::souls::household::HouseholdStock;
use crate::souls::human::HumanDecisionKind;
use crate::transportation::TransportGrid;
use crate::transportation::{
    put_pedestrian_in_transport_grid, unpark, DrivewayTraffic, Location, VehicleKind, VehicleState,
};
use crate::utils::resources::Resources;
use crate::world::{CompanyID, HumanEnt, HumanID, VehicleEnt, VehicleID};
use crate::{HumanWork, ParCommandBuffer, SoulID, World};
use egui_inspect::Inspect;
use geom::{Transform, Vec3};
use prototypes::{Tick, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};
use slotmapd::HopSlotMap;
use std::collections::BTreeSet;

#[path = "transit_path.rs"]
mod transit_path;
use transit_path::{TransitPaths, ValidatedWalk};

const PERSONAL_CAR_ACCESS_RADIUS: f32 = 4.0 * NEAR_PARKING_RADIUS;
const PROPERTY_REQUESTS_PER_TICK: usize = 32;
const PROPERTY_FEASIBILITY_CHECKS: usize = 8;
// Thirty-two admitted requests can spend at most 32,768 route work units.
const PROPERTY_ROUTE_WORK: usize = 1_024;

struct PropertyBudget {
    checks: usize,
    work: usize,
    exhausted: bool,
}

impl PropertyBudget {
    fn new() -> Self {
        Self {
            checks: PROPERTY_FEASIBILITY_CHECKS,
            work: PROPERTY_ROUTE_WORK,
            exhausted: false,
        }
    }

    fn check(&mut self) -> Option<()> {
        if self.checks == 0 || self.work == 0 {
            self.exhausted = true;
            return None;
        }
        self.checks -= 1;
        self.work -= 1;
        Some(())
    }

    fn drive(
        &mut self,
        map: &Map,
        from: DirectedEndpoint,
        to: DirectedEndpoint,
    ) -> Option<DirectedPlan> {
        match DirectedSearch::new(from, to).advance(map, &mut self.work) {
            DirectedSearchResult::Feasible(plan) => Some(plan),
            DirectedSearchResult::Unavailable => None,
            DirectedSearchResult::Deferred | DirectedSearchResult::BudgetExhausted => {
                self.exhausted = true;
                None
            }
        }
    }

    fn error(&self) -> Option<RouterError> {
        (self.exhausted || self.work == 0).then_some(RouterError::PropertyParkingBudgetExhausted)
    }
}

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
    #[serde(skip)]
    trip_origin: Option<Destination>,
    #[inspect(skip)]
    pub(crate) trip_mode: TripProgress,
    pub(crate) walk_only_pickup: bool,
    replan_at_alight: bool,
    #[serde(default)]
    #[inspect(skip)]
    pub trip_purpose: TripPurpose,
    // Employer for a commute, or for the current stay after attending work.
    #[inspect(skip)]
    #[serde(default)]
    trip_employer: Option<CompanyID>,
    #[inspect(skip)]
    #[serde(skip)]
    transit_walks: Option<Box<[Option<ValidatedWalk>; 2]>>,
    #[inspect(skip)]
    #[serde(skip)]
    drive_check: Option<Box<DriveCheck>>,
}

/// The last `can_drive` answer for a car waiting for a route. It holds until the car or its
/// target moves or the map's lanes change.
struct DriveCheck {
    lanes: (u64, u64, u64),
    from: [u32; 3],
    to: [u32; 3],
    can_drive: bool,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum RouterError {
    ReservingParkingSpot(ParkingReserveError),
    TranslatingParkingSpotToDrivePos,
    LocatingVehicle,
    PropertyParkingBlocked,
    PropertyParkingUnavailable(BuildingID),
    PropertyParkingBudgetExhausted,
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
    DriveProperty(VehicleID, Option<Box<DirectedPlan>>, Tick),
    ParkProperty(VehicleID, BuildingID, Tick),
    WalkProperty(Vec3, Box<Itinerary>),
}

debug_inspect_impl!(RoutingStep);

pub fn routing_changed_system(world: &mut World, resources: &mut Resources) {
    let now = elapsed_ticks(resources).0;
    if now % ABANDONED_CAR_SWEEP == 0 {
        recover_abandoned_cars(world, resources);
    }
    if now % 32 == 0 {
        let mut traffic = resources.write::<DrivewayTraffic>();
        for _ in 0..traffic.properties.closing_car_count().min(32) {
            let Some((id, car, site, door)) = traffic.properties.next_closing_owner() else {
                break;
            };
            world.edit_human(id, |human| {
                let router = &mut human.router;
                if router.personal_car == Some(car)
                    && router.selected_vehicle() == Some(car)
                    && router.cur_step.is_none()
                    && router.steps.is_empty()
                    && router.target_dest == router.cur_dest
                    && !matches!(human.location, Location::Transit(_) | Location::Vehicle(_))
                {
                    let goal = if human.home.house != site
                        && human.location != Location::Building(human.home.house)
                    {
                        Destination::Building(human.home.house)
                    } else {
                        Destination::Outside(door)
                    };
                    router.reset_dest();
                    router.go_to(goal);
                    human.decision.wait = u8::MAX;
                }
            });
        }
    }
    routing_changed(world, resources, Transit::headway);
}

fn parking_eligible(
    map: &Map,
    infos: &BuildingInfos,
    driver: HumanID,
    site: BuildingID,
    target: Option<Destination>,
    purpose: TripPurpose,
    workplace: Option<BuildingID>,
    basket: Option<SoulID>,
) -> bool {
    let Some(building) = map.get(site) else {
        return false;
    };
    if matches!(building.kind, crate::map::BuildingKind::ParkingGarage(_)) {
        return true;
    }
    if target != Some(Destination::Building(site)) || building.is_abandoned_growable() {
        return false;
    }
    if infos.residence_of(SoulID::Human(driver)) == Some(site) {
        return true;
    }
    if building.development().is_some_and(|development| {
        matches!(
            development.business,
            Some(crate::map::GrowableBusinessState::ClosedUntil { .. })
        )
    }) {
        return false;
    }
    (purpose == TripPurpose::Work && workplace == Some(site))
        || (purpose == TripPurpose::Shopping
            && crate::souls::residence::is_retail_store(building.kind)
            && basket.is_some_and(|seller| infos.building_owned_by(seller) == Some(site)))
}

fn active_parking_basket(
    household: &HouseholdStock,
    driver: HumanID,
    resources: &Resources,
) -> Option<SoulID> {
    household
        .reservation
        .as_ref()
        .filter(|offer| offer.shopper == driver && offer.expires > elapsed_ticks(resources).0)
        .map(|offer| offer.seller)
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
    let mut driveways = resources.write::<DrivewayTraffic>();
    let has_properties = !driveways.properties.is_empty();
    let mut property_requests = 0;

    world.humans.edit_active(HumanWork::RouteChanged, |id, h| {
        let h: &mut HumanEnt = h;
        let router = &mut h.router;
        if matches!(h.location, Location::Transit(_)) || h.it.is_pedestrian_egress() {
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
            if router.cur_step.is_none() && router.steps.is_empty() {
                // The trip was cancelled while driving: take the car off the road first.
                park_idle_car(
                    id,
                    h,
                    map,
                    parking,
                    &world.vehicles,
                    &cbuf_human,
                    &cbuf_vehicle,
                );
            }
            return;
        }
        let dest = unwrap_ret!(router.target_dest);
        let mut checked_property = false;
        if has_properties {
            if router
                .personal_car
                .is_some_and(|car| driveways.properties.is_moving(car))
            {
                return;
            }
            if let Some(car) = router.personal_car {
                let destination_has_site = matches!(dest, Destination::Building(site)
                    if driveways.properties.has_site(site));
                checked_property = router.vehicle == Some(car)
                    && !router.walk_only_pickup
                    && (destination_has_site
                        || driveways.properties.parked_at(car).is_some()
                        || matches!(
                            router.last_error,
                            Some(RouterError::PropertyParkingUnavailable(_))
                        ));
                if checked_property {
                    if property_requests >= PROPERTY_REQUESTS_PER_TICK {
                        return;
                    }
                    property_requests += 1;
                }
                driveways.properties.release_arrival(car);
            }
        }

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
                    router.trip_employer = None;
                    on_cancel(router);
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
            if router.trip_purpose == TripPurpose::Home {
                let employer = return_home_employer(h, dest, resources);
                h.router.trip_employer = employer;
            }
            h.router.trip_start = Some(elapsed_ticks(resources));
            h.router.trip_origin = Some(match h.location {
                Location::Building(building) => Destination::Building(building),
                Location::Vehicle(vehicle) => Destination::Outside(
                    world.vehicles.get(vehicle).map_or(h.trans.pos, |v| v.trans.pos),
                ),
                _ => Destination::Outside(h.trans.pos),
            });
        }
        let router = &mut h.router;

        let mut transit_steps = None;
        if !transit.runs.is_empty()
            && router.allows_transit(&h.household)
            && !matches!(h.location, Location::Vehicle(_))
            && (router.vehicle.is_none() || router.vehicle == router.personal_car)
            && !h
                .work
                .as_ref()
                .is_some_and(|work| matches!(work.kind, WorkKind::Driver { .. }))
        {
            let car = router.vehicle.and_then(|id| world.vehicles.get(id));
            let recovering_car = matches!(h.decision.kind, HumanDecisionKind::DepositAtHome)
                && car
                    .is_some_and(|car| car.trans.pos.distance2(pos) > NEAR_PARKING_RADIUS.powi(2));
            if !recovering_car
                && car.is_none_or(|vehicle| matches!(vehicle.vehicle.kind, VehicleKind::Car))
            {
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
                        nearby_car,
                        walk_speed,
                        |line| transit.allows_line_for_trip(id, router, line),
                    );
                }
            }
        }

        let mut parking_budget = PropertyBudget::new();
        let mut route = if let Some((steps, paths)) = transit_steps {
            router.transit_walks = Some(Box::new(paths.map(Some)));
            Ok(steps)
        } else {
            let property_route = if checked_property {
                let eligible = building.filter(|&site| {
                    parking_eligible(
                        map,
                        &resources.read::<BuildingInfos>(),
                        id,
                        site,
                        router.target_dest,
                        router.trip_purpose,
                        h.work.as_ref().map(|work| work.workplace),
                        active_parking_basket(&h.household, id, resources),
                    )
                });
                router.property_steps(
                    id,
                    h.trans.pos,
                    pos,
                    eligible,
                    true,
                    parking,
                    map,
                    &h.location,
                    &world.vehicles,
                    &mut driveways.properties,
                    &mut parking_budget,
                )
            } else {
                None
            };
            if let Some(steps) = property_route {
                Ok(steps)
            } else if checked_property {
                Err(RouterError::ReservingParkingSpot(
                    ParkingReserveError::NoSpotFoundAfterSearch,
                ))
            } else {
                router.steps_to(pos, parking, map, &h.location, &world.vehicles)
            }
        };
        if matches!(route, Err(RouterError::ReservingParkingSpot(_)))
            && !checked_property
            && driveways.properties.has_public_garages()
            && router.vehicle.is_some()
            && router.vehicle == router.personal_car
            && !router.walk_only_pickup
        {
            if property_requests >= PROPERTY_REQUESTS_PER_TICK {
                return;
            }
            property_requests += 1;
            if let Some(steps) = router.property_steps(
                id, h.trans.pos, pos, None, false, parking, map, &h.location,
                &world.vehicles, &mut driveways.properties, &mut parking_budget,
            ) {
                route = Ok(steps);
            }
        }
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
                        let exit_pos = world.vehicles.get(car)
                            .map_or(h.trans.pos, |v| exit_beside(map, v));
                        walk_outside(id, exit_pos, &cbuf_human, &mut h.location);
                        h.it = Itinerary::NONE;
                        let home = map.get(h.home.house).map(|home| home.door_pos);
                        cbuf_vehicle.exec_ent(car, move |sim| recover_car_later(sim, car, home));
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
                router.last_error = parking_budget.error().or(Some(error));
                return;
            }
        };
        let mode = if router
            .steps
            .iter()
            .any(|step| matches!(step, RoutingStep::RideTransit { .. }))
        {
            TripMode::Transit
        } else if router.steps.iter().any(|step| {
            matches!(
                step,
                RoutingStep::DriveTo(..) | RoutingStep::DriveProperty(..)
            )
        }) {
            TripMode::Car
        } else {
            TripMode::Walk
        };
        router.trip_mode.plan(mode);
        if let Some(building) = building {
            router.steps.push(RoutingStep::GetInBuilding(building));
        }

        router.last_error = parking_budget.error();
        router.cur_dest = router.target_dest;

        router.steps.reverse();
    });
}

// Both GoHome and DepositAtHome use the Home purpose.
fn return_home_employer(
    human: &HumanEnt,
    destination: Destination,
    resources: &Resources,
) -> Option<CompanyID> {
    if !matches!(human.decision.kind, HumanDecisionKind::GoHome(home)
        if destination == Destination::Building(home) && home == human.home.house)
    {
        return None;
    }
    let work = human.work.as_ref()?;
    let employer = human.router.trip_employer?;
    (human.location == Location::Building(work.workplace)
        && work.employer.is_none_or(|assigned| assigned == employer)
        && resources.read::<BuildingInfos>().owner(work.workplace)
            == Some(SoulID::GoodsCompany(employer)))
    .then_some(employer)
}

pub fn routing_update_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::routing_update_system");
    let map: &Map = &resources.read();
    let cbuf_human: &ParCommandBuffer<HumanEnt> = &resources.read();
    let cbuf_vehicle: &ParCommandBuffer<VehicleEnt> = &resources.read();

    world.humans.edit_active(HumanWork::RouteStep, |body, h| {
        if !h.router.has_route_step() || h.it.is_pedestrian_egress() {
            return;
        }

        let trans: &Transform = &h.trans;

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

        if let Some(RoutingStep::DriveTo(car, obj)) = h.router.cur_step {
            if h.location == Location::Vehicle(car) && map.has_junction_rules() {
                if let Some(vehicle) = world.vehicles.get_mut(car) {
                    if vehicle.it.is_missing()
                        && !vehicle.vehicle.state.is_parked()
                        && !vehicle.trans.pos.is_close(obj, OBJECTIVE_OK_DIST)
                        && map.junction_rules_near(vehicle.trans.pos, OBJECTIVE_OK_DIST)
                    {
                        // Keep the current step and any cargo manifest until the
                        // original vehicle reaches this drive objective.
                        vehicle.it = Itinerary::wait_for_reroute(PathKind::Vehicle, obj);
                        return;
                    }
                }
            }
            // Plan again when the personal car cannot reach its parking spot, e.g. after
            // loading a save or when its road lost its connection to the destination.
            if h.router.personal_car == Some(car)
                && world.vehicles.get(car).is_some_and(|v| {
                    v.it.is_wait_for_reroute().is_some()
                        && !h.router.can_drive_cached(map, v.trans.pos, obj)
                })
            {
                if h.router.target_dest.is_none() {
                    // A park plan has no destination to plan again for. Without steps,
                    // the idle car is parked beside its road or its owner gets out.
                    h.router
                        .clear_steps(&mut resources.write::<ParkingManagement>());
                } else {
                    h.router.reset_dest();
                }
                return;
            }
        }

        let itin: &Itinerary = &h.it;
        let mut cur_step_over = true;

        if let Some(ref step) = h.router.cur_step {
            cur_step_over = match *step {
                RoutingStep::WalkTo(_) | RoutingStep::WalkProperty(..) => itin.has_ended(0.0),
                RoutingStep::DriveProperty(car, ref plan, started) => {
                    let Some(actor) = world.vehicles.get_mut(car) else {
                        h.router.reset_dest();
                        return;
                    };
                    let mut traffic = resources.write::<DrivewayTraffic>();
                    let properties = &mut traffic.properties;
                    if plan.is_some() {
                        let now = elapsed_ticks(resources);
                        if now.0.saturating_sub(started.0) > 240 {
                            h.router.last_error = Some(RouterError::PropertyParkingBlocked);
                            h.router.reset_dest();
                            return;
                        }
                        let Some(RoutingStep::DriveProperty(_, plan, _)) = &mut h.router.cur_step
                        else {
                            unreachable!()
                        };
                        let route = plan.as_ref().unwrap();
                        if !route.valid(map) {
                            h.router.last_error = Some(RouterError::PropertyParkingBlocked);
                            h.router.reset_dest();
                            return;
                        }
                        if properties.parked_at(car).is_some() {
                            if !properties.start_departure(
                                &mut resources.write::<TransportGrid>(),
                                car,
                                actor,
                                route.as_ref().clone(),
                            ) {
                                return;
                            }
                        } else {
                            actor.it = route.as_ref().clone().into_itinerary();
                        }
                        *plan = None;
                        h.router.trip_mode.drove();
                        return;
                    }
                    !properties.is_moving(car) && actor.it.has_ended(0.0)
                }
                RoutingStep::ParkProperty(car, site, deadline) => {
                    let Some(actor) = world.vehicles.get_mut(car) else {
                        h.router.reset_dest();
                        return;
                    };
                    let mut traffic = resources.write::<DrivewayTraffic>();
                    let properties = &mut traffic.properties;
                    if properties.parked_at(car) == Some(site) {
                        true
                    } else if properties.is_moving(car) {
                        false
                    } else if elapsed_ticks(resources) >= deadline
                        || h.router.personal_car != Some(car)
                        || !properties.has_arrival(car, site, body)
                        || !parking_eligible(
                            map,
                            &resources.read::<BuildingInfos>(),
                            body,
                            site,
                            h.router.target_dest,
                            h.router.trip_purpose,
                            h.work.as_ref().map(|work| work.workplace),
                            active_parking_basket(&h.household, body, resources),
                        )
                    {
                        properties.release_arrival(car);
                        h.router.last_error = Some(RouterError::PropertyParkingUnavailable(site));
                        h.router.reset_dest();
                        return;
                    } else {
                        properties.start_arrival(
                            map,
                            &mut resources.write::<TransportGrid>(),
                            car,
                            actor,
                        );
                        false
                    }
                }
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
                RoutingStep::WalkTo(_) | RoutingStep::WalkProperty(..) => true,
                RoutingStep::DriveTo(_, _)
                | RoutingStep::DriveProperty(..)
                | RoutingStep::ParkProperty(..) => true,
                RoutingStep::Park(_, _) => true,
                // A parking manoeuvre finishes before the car can leave its spot.
                RoutingStep::Unpark(vehicle) => world
                    .vehicles
                    .get(vehicle)
                    .is_none_or(|v| !matches!(v.vehicle.state, VehicleState::RoadToPark(..))),
                RoutingStep::GetInVehicle(vehicle) => world
                    .vehicles
                    .get(vehicle)
                    .map(|v| {
                        let pickup = resources
                            .read::<DrivewayTraffic>()
                            .properties
                            .pedestrian_gate(vehicle)
                            .unwrap_or(v.trans.pos);
                        pickup.is_close(pos, 3.0)
                    })
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

        let h: &mut HumanEnt = h;
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
                    if h.router.walk_only_pickup && h.household.on_errand() {
                        route = Itinerary::route(
                            elapsed_ticks(resources),
                            pos,
                            obj,
                            map,
                            PathKind::Pedestrian,
                        );
                        if route.is_none() {
                            h.router
                                .cancel_route(&mut resources.write::<ParkingManagement>());
                            h.it = Itinerary::NONE;
                            h.decision.wait = 0;
                            return;
                        }
                    }
                    h.it = route
                        .unwrap_or_else(|| Itinerary::wait_for_reroute(PathKind::Pedestrian, obj));
                }
                RoutingStep::WalkProperty(target, ref mut plan) => {
                    h.it = std::mem::take(plan.as_mut());
                    if h.it.is_none_or_wait() {
                        h.it = Itinerary::wait_for_reroute(PathKind::Pedestrian, target);
                    }
                }
                RoutingStep::DriveProperty(_, _, ref mut started) => {
                    *started = elapsed_ticks(resources);
                }
                RoutingStep::ParkProperty(_, _, ref mut deadline) => {
                    *deadline = Tick(elapsed_ticks(resources).0.saturating_add(240));
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
                    cbuf_vehicle.exec_ent(vehicle, move |sim| {
                        if sim
                            .world
                            .vehicles
                            .get(vehicle)
                            .is_some_and(|car| car.vehicle.state.is_parked())
                        {
                            unpark(sim, vehicle);
                        }
                    });
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
                    let gate = resources
                        .read::<DrivewayTraffic>()
                        .properties
                        .pedestrian_gate(vehicle);
                    let pos = gate
                        .or_else(|| {
                            world
                                .vehicles
                                .get(vehicle)
                                .map(|v| v.trans.pos + v.trans.dir.cross(Vec3::Z) * 2.0)
                        })
                        .unwrap_or(pos);
                    if h.router.personal_car == Some(vehicle) {
                        // Nobody would move a personal car left on the road again.
                        recover_abandoned_car(
                            &mut world.vehicles,
                            map,
                            &mut resources.write(),
                            resources,
                            vehicle,
                            map.get(h.home.house).map(|home| home.door_pos),
                        );
                    }
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
                    h.router.trip_employer = None;
                    match h.router.trip_purpose {
                        TripPurpose::Work => h.attend_work_for(
                            &resources.read::<prototypes::GameTime>(),
                            build,
                            &resources.read::<BuildingInfos>(),
                        ),
                        TripPurpose::Shopping | TripPurpose::Leisure => h.decision.wait = 0,
                        TripPurpose::Home if !h.household.carried.is_empty() => h.decision.wait = 0,
                        _ => {}
                    }
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
        } else if matches!(h.router.cur_dest, Some(Destination::Outside(_))) {
            on_arrival(&mut h.router, resources);
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
        sim.world.edit_human(body, |h| {
            h.trans.pos = pos;
            h.collider = Some(coll);
        });
    });
}

/// Personal cars standing on the road with no route left and nobody inside or on the way to
/// them, with their owners. Nothing moves such a car, so it blocks its lane for good.
pub(crate) fn abandoned_cars(world: &World) -> Vec<(VehicleID, HumanID)> {
    let riding: BTreeSet<VehicleID> = world
        .humans
        .values()
        .filter_map(|h| match h.location {
            Location::Vehicle(id) => Some(id),
            _ => None,
        })
        .collect();
    world
        .humans
        .iter()
        .filter_map(|(owner, h)| {
            let car = h.router.personal_car?;
            let vehicle = world.vehicles.get(car)?;
            (on_road(vehicle).is_some()
                && vehicle.it.is_terminal()
                && vehicle.it.driveway_phase().is_none()
                && !riding.contains(&car)
                && !h.router.plans_with(car))
            .then_some((car, owner))
        })
        .collect()
}

/// How often personal cars left on the road are looked for, e.g. in saves from before cancelled
/// trips parked the car. Nothing in the simulation leaves one there otherwise.
const ABANDONED_CAR_SWEEP: u64 = TICKS_PER_MINUTE;

fn recover_abandoned_cars(world: &mut World, resources: &Resources) {
    let abandoned = abandoned_cars(world);
    if abandoned.is_empty() {
        return;
    }
    log::warn!(
        "taking {} abandoned personal cars off the road",
        abandoned.len()
    );
    let map = resources.read::<Map>();
    let mut parking = resources.write::<ParkingManagement>();
    for (car, owner) in abandoned {
        let home = world
            .humans
            .get(owner)
            .and_then(|owner| map.get(owner.home.house))
            .map(|home| home.door_pos);
        recover_abandoned_car(
            &mut world.vehicles,
            &map,
            &mut parking,
            resources,
            car,
            home,
        );
    }
}

/// Takes a personal car nobody drives off the road: it parks in a spot near where it stands,
/// otherwise it is moved to a spot near its owner's home. A car with neither is removed.
fn recover_abandoned_car(
    vehicles: &mut HopSlotMap<VehicleID, VehicleEnt>,
    map: &Map,
    parking: &mut ParkingManagement,
    resources: &Resources,
    car: VehicleID,
    home: Option<Vec3>,
) {
    let Some(vehicle) = vehicles.get_mut(car) else {
        return;
    };
    let Some(pos) = on_road(vehicle) else {
        return;
    };
    vehicle.it = Itinerary::NONE;
    if let Some(spot) = reserve_beside(pos, parking, map) {
        if let Some(trans) = spot.get(&map.parking).map(|spot| spot.trans) {
            vehicle.begin_parking(trans, spot);
            return;
        }
        parking.free(spot);
    }
    if let Some(spot) = home.and_then(|door| parking.reserve_near(door, map).ok()) {
        if let Some(trans) = spot.get(&map.parking).map(|spot| spot.trans) {
            if let Some(collider) = vehicle.collider.take() {
                resources
                    .write::<TransportGrid>()
                    .remove_maintain(collider.0);
            }
            vehicle.trans = trans;
            vehicle.speed.0 = 0.0;
            vehicle.vehicle.state = VehicleState::Parked(spot);
            return;
        }
        parking.free(spot);
    }
    resources.read::<ParCommandBuffer<VehicleEnt>>().kill(car);
}

/// A free spot within [`NEAR_PARKING_RADIUS`] beside the road a car stands on. Parking glides
/// the car straight into its spot, so a spot on another road would take it across a block.
fn reserve_beside(
    pos: Vec3,
    parking: &mut ParkingManagement,
    map: &Map,
) -> Option<SpotReservation> {
    let road = map.get(PathKind::Vehicle.nearest_lane(map, pos)?)?.parent;
    let spot = parking
        .reserve_near_radius(pos, map, NEAR_PARKING_RADIUS)
        .ok()?;
    let beside = spot
        .get(&map.parking)
        .and_then(|spot| map.get(spot.parent))
        .is_some_and(|lane| lane.parent == road);
    if beside {
        Some(spot)
    } else {
        parking.free(spot);
        None
    }
}

/// Steps that park a personal car in a spot beside its road and let its driver out.
fn park_beside(
    car: VehicleID,
    pos: Vec3,
    parking: &mut ParkingManagement,
    map: &Map,
) -> Option<Vec<RoutingStep>> {
    let here = reserve_beside(pos, parking, map)?;
    Some(vec![
        RoutingStep::Park(car, Some(here)),
        RoutingStep::GetOutVehicle(car),
    ])
}

/// Steps that take a personal car off the road near where it stands and let its driver out.
/// A spot beside its road is parked in directly; otherwise the car drives to the nearest spot
/// it can reach. Only trips with a destination use the drive: a stuck drive replans for it.
fn park_where_it_stands(
    car: VehicleID,
    pos: Vec3,
    parking: &mut ParkingManagement,
    map: &Map,
) -> Option<Vec<RoutingStep>> {
    if let Some(steps) = park_beside(car, pos, parking, map) {
        return Some(steps);
    }
    let spot = parking.reserve_near(pos, map).ok()?;
    match spot.park_pos(map).filter(|&to| can_drive(map, pos, to)) {
        Some(to) => Some(vec![
            RoutingStep::DriveTo(car, to),
            RoutingStep::Park(car, Some(spot)),
            RoutingStep::GetOutVehicle(car),
        ]),
        None => {
            parking.free(spot);
            None
        }
    }
}

/// Parks the personal car a human sits in once their trip is cancelled. Without a spot beside
/// its road the human gets out and the car is taken off the road like an abandoned one.
fn park_idle_car(
    id: HumanID,
    h: &mut HumanEnt,
    map: &Map,
    parking: &mut ParkingManagement,
    vehicles: &HopSlotMap<VehicleID, VehicleEnt>,
    cbuf_human: &ParCommandBuffer<HumanEnt>,
    cbuf_vehicle: &ParCommandBuffer<VehicleEnt>,
) {
    let Location::Vehicle(car) = h.location else {
        return;
    };
    if h.router.personal_car != Some(car) {
        return;
    }
    let Some(vehicle) = vehicles
        .get(car)
        .filter(|vehicle| on_road(vehicle).is_some())
    else {
        return;
    };
    if let Some(mut steps) = park_beside(car, vehicle.trans.pos, parking, map) {
        steps.reverse();
        h.router.steps = steps;
        return;
    }
    walk_outside(id, exit_beside(map, vehicle), cbuf_human, &mut h.location);
    h.it = Itinerary::NONE;
    let home = map.get(h.home.house).map(|home| home.door_pos);
    cbuf_vehicle.exec_ent(car, move |sim| recover_car_later(sim, car, home));
}

pub(crate) fn recover_car_later(sim: &mut crate::Simulation, car: VehicleID, home: Option<Vec3>) {
    let (world, resources) = sim.world_res();
    recover_abandoned_car(
        &mut world.vehicles,
        &resources.read::<Map>(),
        &mut resources.write::<ParkingManagement>(),
        resources,
        car,
        home,
    );
}

/// Where a driver steps out of a car: on the nearest sidewalk, otherwise beside the car.
fn exit_beside(map: &Map, car: &VehicleEnt) -> Vec3 {
    let pos = car.trans.pos + car.trans.dir.cross(Vec3::Z) * 2.0;
    map.nearest_lane(
        pos,
        crate::map::LaneKind::Walking,
        Some(NEAR_PARKING_RADIUS),
    )
    .and_then(|lane| map.get(lane))
    .map_or(pos, |lane| lane.points.project_segment(pos).0)
}

/// Position of a car that is on a driving lane, as opposed to parked or parking.
fn on_road(vehicle: &VehicleEnt) -> Option<Vec3> {
    (matches!(vehicle.vehicle.kind, VehicleKind::Car)
        && vehicle.it.driveway_phase().is_none()
        && matches!(
            vehicle.vehicle.state,
            VehicleState::Driving | VehicleState::Panicking(_)
        ))
    .then_some(vehicle.trans.pos)
}

pub(crate) type WalkingReach = crate::economy::WalkingComponents;

#[cfg(test)]
thread_local! {
    static CAN_DRIVE_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether a vehicle route can exist between the driving lanes nearest to both positions.
fn can_drive(map: &Map, from: Vec3, to: Vec3) -> bool {
    #[cfg(test)]
    CAN_DRIVE_CHECKS.set(CAN_DRIVE_CHECKS.get() + 1);
    let nearest = |pos| PathKind::Vehicle.nearest_lane(map, pos);
    match (nearest(from), nearest(to)) {
        // Same-lane trips may only need a local path.
        (Some(start), Some(end)) => start == end || map.can_drive_between(start, end),
        _ => false,
    }
}

fn directed_at(map: &Map, pos: Vec3, lane: crate::map::LaneID) -> Option<DirectedEndpoint> {
    let points = &map.get(lane)?.points;
    let (projection, end) = points.project_segment_2d(pos.xy());
    let start = end.checked_sub(1)?;
    let offset = points
        .segments()
        .take(start)
        .map(|segment| segment.src.distance(segment.dst))
        .sum::<f32>()
        + points[start].distance(projection);
    Some(DirectedEndpoint {
        lane,
        offset_cm: (offset * 100.0).floor() as u32,
    })
}

pub(crate) fn walking_route_distance(map: &Map, start: Vec3, end: Vec3) -> Option<f32> {
    let route = Itinerary::route(Tick(0), start, end, map, PathKind::Pedestrian)?;
    let mut pos = start;
    let mut distance = 0.0;
    for &point in route.local_path().iter().rev() {
        distance += pos.distance(point);
        pos = point;
    }
    if let Some(route) = route.get_route() {
        for (index, step) in route.reversed_route.iter().enumerate().rev() {
            if index == 0 {
                let points =
                    PathKind::Pedestrian.local_route(map, step.destination_lane(), pos, end)?;
                distance += pos.distance(points.first()) + points.length();
            } else {
                let points = step.raw_points(map)?;
                let (first, last) = match step.dir {
                    TraverseDirection::Forward => (points.first(), points.last()),
                    TraverseDirection::Backward => (points.last(), points.first()),
                };
                distance += pos.distance(first) + points.length();
                pos = last;
            }
        }
    }
    Some(distance)
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
            trip_origin: None,
            trip_mode: TripProgress::Walk,
            trip_purpose: TripPurpose::Other,
            trip_employer: None,
            walk_only_pickup: false,
            replan_at_alight: false,
            transit_walks: None,
            drive_check: None,
        }
    }

    pub fn cur_step(&self) -> Option<&RoutingStep> {
        self.cur_step.as_ref()
    }

    pub fn trip_origin(&self) -> Option<Destination> {
        self.trip_start.and(self.trip_origin)
    }

    pub(crate) fn restore_trip_origin(&mut self, origin: Destination) {
        if self.trip_start.is_some() {
            self.trip_origin = Some(origin);
        }
    }

    /// `can_drive` for a car that keeps waiting for a route, evaluated again only when its
    /// inputs change: the nearest lanes and their reachability are fixed between lane edits.
    fn can_drive_cached(&mut self, map: &Map, from: Vec3, to: Vec3) -> bool {
        let key = DriveCheck {
            lanes: (
                map.identity(),
                map.walking_topology_generation,
                map.driving_policy_generation,
            ),
            from: <[f32; 3]>::from(from).map(f32::to_bits),
            to: <[f32; 3]>::from(to).map(f32::to_bits),
            can_drive: false,
        };
        if let Some(check) = self
            .drive_check
            .as_deref()
            .filter(|c| c.lanes == key.lanes && c.from == key.from && c.to == key.to)
        {
            return check.can_drive;
        }
        let can_drive = can_drive(map, from, to);
        self.drive_check = Some(Box::new(DriveCheck { can_drive, ..key }));
        can_drive
    }

    /// Legacy goods pickups and errands of car owners stay on foot. A household without a
    /// car may take the bus for its errands.
    pub(crate) fn allows_transit(&self, household: &HouseholdStock) -> bool {
        !self.walk_only_pickup || (self.personal_car.is_none() && household.on_errand())
    }

    pub(crate) fn has_route_step(&self) -> bool {
        self.cur_step.is_some() || !self.steps.is_empty()
    }

    pub(crate) fn needs_route_change(&self, location: &Location) -> bool {
        if matches!(location, Location::Transit(_)) {
            return false;
        }
        if self.replan_at_alight {
            return true;
        }
        if self.cur_dest != self.target_dest {
            return self.target_dest.is_some();
        }
        // Keep the car-state check in the routing pass. Vehicle edits can
        // make a cancelled driver eligible without mutating the human.
        !self.has_route_step()
            && matches!(location, Location::Vehicle(car) if Some(*car) == self.personal_car)
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
        if human.location != Location::Outside || !router.allows_transit(&human.household) {
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

    pub(crate) fn cancel_at_alight(&mut self) {
        on_cancel(self);
        self.target_dest = None;
        self.trip_employer = None;
        self.defer_replan();
    }

    /// Leaves the vehicle of an aborted trip. A personal car on the road is parked first, beside
    /// its road, so that it does not block the lane once its driver is gone. The trip was aborted
    /// because routing failed, so the car is not driven to a farther spot: it is recovered.
    pub(crate) fn exit_aborted_vehicle(
        human: &mut HumanEnt,
        id: VehicleID,
        vehicles: &HopSlotMap<VehicleID, VehicleEnt>,
        map: &Map,
        resources: &Resources,
    ) {
        let personal = human.router.personal_car == Some(id);
        if let Some(pos) = vehicles.get(id).and_then(on_road).filter(|_| personal) {
            let parking = &mut resources.write::<ParkingManagement>();
            if let Some(mut steps) = park_beside(id, pos, parking, map) {
                human.router.clear_steps(parking);
                steps.reverse();
                human.router.steps = steps;
                return;
            }
        }
        let home = map.get(human.home.house).map(|home| home.door_pos);
        let exit = vehicles
            .get(id)
            .map_or(human.trans.pos, |car| exit_beside(map, car));
        resources
            .read::<ParCommandBuffer<VehicleEnt>>()
            .exec_ent(id, move |sim| {
                if let Some(vehicle) = sim.world.vehicles.get_mut(id) {
                    vehicle.it = Itinerary::NONE;
                }
                if personal {
                    recover_car_later(sim, id, home);
                }
            });
        Self::restore_walking(human, exit, resources);
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
        self.steps
            .iter()
            .chain(self.cur_step())
            .any(|step| match *step {
                RoutingStep::WaitAtStop { stop: id, .. }
                | RoutingStep::RideTransit { alight: id, .. } => id == stop,
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

    pub fn is_returning_home_from_work(&self, employer: CompanyID, home: BuildingID) -> bool {
        self.trip_purpose == TripPurpose::Home
            && self.trip_employer == Some(employer)
            && self.target_dest == Some(Destination::Building(home))
            && self.cur_dest == self.target_dest
            && self.last_error.is_none()
            && self.has_route_step()
    }

    pub(crate) fn commutes_for(&self, employer: CompanyID) -> bool {
        self.target_dest.is_some()
            && self.trip_purpose == TripPurpose::Work
            && self.trip_employer == Some(employer)
    }

    #[cfg(test)]
    pub(crate) fn references_building(&self, building: BuildingID) -> bool {
        self.references_any_building(|id| id == building)
    }

    pub(crate) fn references_any_building(&self, includes: impl Fn(BuildingID) -> bool) -> bool {
        self.target_dest.into_iter().chain(self.cur_dest).any(|dest| {
            matches!(dest, Destination::Building(id) if includes(id))
        }) || self.steps.iter().chain(self.cur_step.iter()).any(|step| {
            matches!(step, RoutingStep::GetInBuilding(id) | RoutingStep::GetOutBuilding(id) if includes(*id))
        })
    }

    /// Whether a planned or current step uses the vehicle.
    fn plans_with(&self, id: VehicleID) -> bool {
        self.steps
            .iter()
            .chain(self.cur_step.iter())
            .any(|step| match *step {
                RoutingStep::DriveTo(vehicle, _)
                | RoutingStep::DriveProperty(vehicle, ..)
                | RoutingStep::ParkProperty(vehicle, ..)
                | RoutingStep::Park(vehicle, _)
                | RoutingStep::Unpark(vehicle)
                | RoutingStep::GetInVehicle(vehicle)
                | RoutingStep::GetOutVehicle(vehicle) => vehicle == id,
                _ => false,
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
        selected || self.plans_with(id)
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
        self.trip_employer = None;
        self.last_error = None;
        self.replan_at_alight = false;
    }

    pub fn reset_dest(&mut self) {
        self.cur_dest = None;
    }

    /// Returns wheter or not the destination was already attained
    pub fn go_to(&mut self, dest: Destination) -> bool {
        self.go_to_for(dest, TripPurpose::Other)
    }

    pub(crate) fn go_to_work(
        &mut self,
        workplace: BuildingID,
        employer: Option<CompanyID>,
    ) -> bool {
        let dest = Destination::Building(workplace);
        if self.target_dest == Some(dest)
            && self.cur_dest == Some(dest)
            && self.trip_start.is_none()
        {
            // Work arrival has already recorded whether this stay includes attendance.
            return !self.has_route_step();
        }
        let arrived = self.go_to_for(dest, TripPurpose::Work);
        self.trip_employer = employer;
        arrived
    }

    pub(crate) fn record_work_stay(&mut self, employer: CompanyID) {
        self.trip_employer = Some(employer);
    }

    pub(crate) fn go_home(&mut self, home: BuildingID) -> bool {
        let stay_employer = self.trip_employer.filter(|_| {
            self.trip_start.is_none()
                && matches!(self.trip_purpose, TripPurpose::Other | TripPurpose::Work)
        });
        let arrived = self.go_to_for(Destination::Building(home), TripPurpose::Home);
        if stay_employer.is_some() {
            self.trip_employer = stay_employer;
        }
        arrived
    }

    pub(crate) fn go_to_for(&mut self, dest: Destination, purpose: TripPurpose) -> bool {
        if self.target_dest == Some(dest) {
            if self.trip_purpose != purpose
                && matches!(purpose, TripPurpose::Home | TripPurpose::Work)
            {
                self.trip_purpose = purpose;
                self.trip_employer = None;
            }
            return self.cur_dest == Some(dest) && self.steps.is_empty() && self.cur_step.is_none();
        }
        if self.cur_dest == Some(dest) {
            self.cur_dest = None;
        }
        self.target_dest = Some(dest);
        self.trip_purpose = purpose;
        self.trip_employer = None;
        crate::diagnostics::record_trip_offer(purpose);
        on_start(self);
        false
    }

    pub(crate) fn has_property_arrival(&self, car: VehicleID, site: BuildingID) -> bool {
        self.steps.iter().chain(self.cur_step.iter()).any(
            |step| matches!(step, RoutingStep::ParkProperty(v, b, _) if *v == car && *b == site),
        )
    }

    fn property_steps(
        &self,
        driver: HumanID,
        pedestrian: Vec3,
        destination: Vec3,
        building: Option<BuildingID>,
        allow_shared: bool,
        parking: &mut ParkingManagement,
        map: &Map,
        location: &Location,
        cars: &HopSlotMap<VehicleID, VehicleEnt>,
        properties: &mut BuildingParking,
        budget: &mut PropertyBudget,
    ) -> Option<Vec<RoutingStep>> {
        let car = self
            .personal_car
            .filter(|car| self.vehicle == Some(*car) && !self.walk_only_pickup)?;
        let actor = cars.get(car)?;
        let source = properties.parked_at(car);
        let mut steps = Vec::new();
        let pedestrian = if let Location::Building(id) = *location {
            steps.push(RoutingStep::GetOutBuilding(id));
            map.get(id)?.door_pos
        } else {
            pedestrian
        };
        let walk = || {
            let mut steps = Vec::new();
            if let Location::Building(id) = *location {
                steps.push(RoutingStep::GetOutBuilding(id));
            } else if *location == Location::Vehicle(car) {
                steps.push(RoutingStep::GetOutVehicle(car));
            }
            steps.push(RoutingStep::WalkTo(destination));
            steps
        };
        let fallback = || source.filter(|_| allow_shared).map(|_| walk());
        let retry = matches!(self.last_error, Some(RouterError::PropertyParkingBlocked));
        let from = properties
            .release(car)
            .or_else(|| match &actor.vehicle.state {
                VehicleState::Parked(spot) | VehicleState::RoadToPark(_, _, spot) => {
                    directed_at(map, spot.park_pos(map)?, map.parking_to_drive(spot.id())?)
                }
                _ => {
                    let pos = actor.trans.pos;
                    directed_at(
                        map,
                        pos,
                        map.nearest_lane(pos, crate::map::LaneKind::Driving, Some(32.0))?,
                    )
                }
            });
        let Some(origin) = from else {
            return fallback();
        };

        if *location != Location::Vehicle(car) {
            if let Some(gate) = properties.pedestrian_gate(car) {
                let Some(pickup) = Itinerary::property_walk(
                    pedestrian, gate, map, &mut budget.work,
                ) else {
                    return fallback();
                };
                steps.push(RoutingStep::WalkProperty(gate, Box::new(pickup)));
            } else {
                steps.push(RoutingStep::WalkTo(actor.trans.pos));
            }
            steps.push(RoutingStep::GetInVehicle(car));
        }
        if source.is_none()
            && matches!(
                actor.vehicle.state,
                VehicleState::Parked(_) | VehicleState::RoadToPark(..)
            )
        {
            steps.push(RoutingStep::Unpark(car));
        }

        let garage = |site, properties: &mut BuildingParking, budget: &mut PropertyBudget| {
            if Some(site) == source
                || matches!(self.last_error, Some(RouterError::PropertyParkingUnavailable(id)) if id == site)
            {
                return None;
            }
            budget.check()?;
            let (target, gate) = properties.arrival_target(site, car)?;
            let egress = Itinerary::property_walk(gate, destination, map, &mut budget.work)?;
            let drive = budget.drive(map, origin, target)?;
            properties.hold(site, driver, car).then_some((
                drive,
                RoutingStep::ParkProperty(car, site, Tick(0)),
                RoutingStep::WalkProperty(destination, Box::new(egress)),
            ))
        };
        let mut selected = building
            .filter(|_| !retry)
            .and_then(|site| garage(site, properties, budget));
        let mut street = Vec::new();
        let bay = |spot, parking: &mut ParkingManagement, budget: &mut PropertyBudget| {
            budget.check()?;
            let target = directed_at(
                map,
                map.parking_to_drive_pos(spot)?,
                map.parking_to_drive(spot)?,
            )?;
            let park = map.parking.get(spot)?.trans;
            let gate = park.pos + park.dir.cross(Vec3::Z) * 2.0;
            let egress = Itinerary::property_walk(gate, destination, map, &mut budget.work)?;
            let drive = budget.drive(map, origin, target)?;
            Some((
                drive,
                RoutingStep::Park(car, Some(parking.reserve_exact(spot)?)),
                RoutingStep::WalkProperty(destination, Box::new(egress)),
            ))
        };
        if selected.is_none() && allow_shared {
            // Check reachability before taking the curb reservation, using the lane
            // alongside the bay so a parallel road cannot supply the approach.
            let (curb, other_streets) = parking.property_curb_candidates(map, destination);
            street = other_streets;
            selected = curb
                .into_iter()
                .take(3)
                .find_map(|spot| bay(spot, parking, budget));
        }
        if selected.is_none() {
            selected = properties
                .nearby_public_garages(destination)
                .into_iter()
                .filter(|&site| Some(site) != building)
                .take(3)
                .find_map(|site| garage(site, properties, budget));
        }
        if selected.is_none() {
            selected = street.into_iter().find_map(|spot| bay(spot, parking, budget));
        }
        let Some((plan, park, egress)) = selected else {
            return fallback();
        };
        steps.push(RoutingStep::DriveProperty(
            car,
            Some(Box::new(plan)),
            Tick(0),
        ));
        steps.push(park);
        steps.push(RoutingStep::GetOutVehicle(car));
        steps.push(egress);
        Some(steps)
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
        let planned = self.push_steps_to(&mut steps, obj, parking, map, loc, cars);
        match planned {
            Ok(()) => Ok(steps),
            Err(error) => {
                for step in steps {
                    if let RoutingStep::Park(_, Some(spot)) = step {
                        parking.free(spot);
                    }
                }
                Err(error)
            }
        }
    }

    fn push_steps_to(
        &mut self,
        steps: &mut Vec<RoutingStep>,
        obj: Vec3,
        parking: &mut ParkingManagement,
        map: &Map,
        loc: &Location,
        cars: &HopSlotMap<VehicleID, VehicleEnt>,
    ) -> Result<(), RouterError> {
        if let Location::Building(cur_build) = loc {
            steps.push(RoutingStep::GetOutBuilding(*cur_build));
        }

        let vehicle = if self.walk_only_pickup {
            None
        } else {
            self.vehicle
        };
        if let Location::Vehicle(current) = *loc {
            if vehicle != Some(current) {
                // Park a personal car before leaving it, so it does not block the lane.
                let park = cars
                    .get(current)
                    .and_then(on_road)
                    .filter(|_| self.personal_car == Some(current))
                    .and_then(|pos| park_where_it_stands(current, pos, parking, map));
                match park {
                    Some(park) => steps.extend(park),
                    None => steps.push(RoutingStep::GetOutVehicle(current)),
                }
            }
        }

        if let Some(car) = vehicle {
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
                    if self.personal_car == Some(car) && !can_drive(map, pos, parking_pos) {
                        // An unparked car would wait on the road forever for a route.
                        parking.free(spot_resa);
                        steps.push(RoutingStep::WalkTo(obj));
                        return Ok(());
                    }
                    steps.push(RoutingStep::WalkTo(pos));
                    steps.push(RoutingStep::GetInVehicle(car));
                    steps.push(RoutingStep::Unpark(car));
                } else {
                    parking.free(spot_resa);
                    self.vehicle = None;
                    return Err(RouterError::LocatingVehicle);
                }
            } else if let Some(parked) = cars.get(car).filter(|v| {
                matches!(
                    v.vehicle.state,
                    VehicleState::Parked(_) | VehicleState::RoadToPark(..)
                )
            }) {
                if self.personal_car == Some(car) && !can_drive(map, parked.trans.pos, parking_pos)
                {
                    // The car cannot reach the destination: leave it parked and walk.
                    parking.free(spot_resa);
                    steps.push(RoutingStep::GetOutVehicle(car));
                    steps.push(RoutingStep::WalkTo(obj));
                    return Ok(());
                }
                // Replanned while parking or before getting out: leave the spot first.
                steps.push(RoutingStep::Unpark(car));
            } else if let Some(pos) = cars
                .get(car)
                .filter(|v| {
                    matches!(
                        v.vehicle.state,
                        VehicleState::Driving | VehicleState::Panicking(_)
                    )
                })
                .map(|v| v.trans.pos)
                .filter(|&pos| self.personal_car == Some(car) && !can_drive(map, pos, parking_pos))
            {
                // Park where the car stands instead of waiting on the road for a route.
                parking.free(spot_resa);
                let park = park_where_it_stands(car, pos, parking, map).ok_or(
                    RouterError::ReservingParkingSpot(ParkingReserveError::NoSpotFoundAfterSearch),
                )?;
                steps.extend(park);
                steps.push(RoutingStep::WalkTo(obj));
                return Ok(());
            }

            steps.push(RoutingStep::DriveTo(car, parking_pos));
            steps.push(RoutingStep::Park(car, Some(spot_resa)));
            steps.push(RoutingStep::GetOutVehicle(car));
        }

        steps.push(RoutingStep::WalkTo(obj));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::LaneKind;
    use crate::map_dynamic::{BusRun, TransitLine, TransitStop};
    use crate::souls::desire::Work;
    use crate::souls::household::BasketId;
    use crate::souls::human::spawn_human;
    use crate::tests::TestCtx;
    use crate::transportation::{spawn_driving_vehicle, VehicleKind};
    use crate::world_command::WorldCommand;
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Color};
    use prototypes::{ItemID, Money};

    #[test]
    fn vehicle_destination_mid_commute_origin_survives_save() {
        let (mut ctx, human, car, workplace) = traffic1_stranded_driver();
        let origin = Destination::Outside(vec3(85.0, 100.0, 0.0));
        ctx.g.world.edit_human(human, |citizen| {
            citizen.location = Location::Vehicle(car);
            citizen.router.trip_start = Some(Tick(5));
            citizen.router.trip_origin = Some(origin);
            citizen.router.target_dest = Some(Destination::Outside(workplace));
            citizen.router.trip_purpose = TripPurpose::Work;
        });

        let bytes = Bincode::encode(&ctx.g).unwrap();
        let saved: crate::SimulationDeser = Bincode::decode(&bytes).unwrap();
        let mut legacy: crate::SimulationDeser = Bincode::decode(&bytes).unwrap();
        legacy.res.remove("trip_origins");
        let legacy = legacy.into_sim().unwrap();
        assert_eq!(legacy.get(human).unwrap().router.trip_start, Some(Tick(5)));
        assert_eq!(legacy.get(human).unwrap().router.trip_origin(), None);

        let restored = saved.into_sim().unwrap();
        let citizen = restored.get(human).unwrap();
        assert_eq!(citizen.location, Location::Vehicle(car));
        assert_eq!(
            citizen.router.target_dest,
            Some(Destination::Outside(workplace))
        );
        assert_eq!(citizen.router.trip_origin(), Some(origin));
    }

    #[test]
    fn e1_fix_replacing_a_pending_destination_resets_trip_purpose() {
        let mut router = Router::new(None);
        let first = Destination::Outside(vec3(10.0, 10.0, 0.0));
        let second = Destination::Outside(vec3(20.0, 20.0, 0.0));
        router.set_test_route(first, vec![RoutingStep::WalkTo(vec3(10.0, 10.0, 0.0))]);
        router.go_to_for(second, TripPurpose::Shopping);
        router.trip_start = Some(Tick(15));
        router.go_to(first);
        assert_eq!(router.target_dest, Some(first));
        assert_eq!(router.trip_purpose, TripPurpose::Other);
        assert!(router.trip_start.is_none());
        assert!(router.cur_dest.is_none());
    }

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
            let ctx = TestCtx::new();
            ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(900.0, 100.0, 0.0)]);
            Self::serving(ctx, |_, origin| origin + Vec3::X * distance)
        }

        /// A two-stop line from the sidewalk at (200, 100) to `destination(origin)`.
        fn serving(mut ctx: TestCtx, destination: impl FnOnce(&TestCtx, Vec3) -> Vec3) -> Self {
            let house = ctx.build_house_near(vec2(200.0, 100.0));
            let human = spawn_human(&mut ctx.g, house).unwrap();
            let origin = Self::sidewalk_at(&ctx, vec3(200.0, 100.0, 0.0));
            let destination = destination(&ctx, origin);
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

    /// A personal car parked on a road that has no driving connection to the destination.
    fn traffic1_owner_with_car_on(
        car_road_y: f32,
        destination_y: f32,
    ) -> (TestCtx, HumanID, VehicleID, Vec3) {
        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(900.0, 100.0, 0.0)]);
        ctx.build_roads(&[vec3(100.0, 400.0, 0.0), vec3(900.0, 400.0, 0.0)]);
        let house = ctx.build_house_near(vec2(300.0, 100.0));
        let human = spawn_human(&mut ctx.g, house).unwrap();
        let car = crate::transportation::spawn_parked_vehicle(
            &mut ctx.g,
            VehicleKind::Car,
            vec3(300.0, car_road_y, 0.0),
        )
        .unwrap();
        let destination = RouteFixture::sidewalk_at(&ctx, vec3(500.0, destination_y, 0.0));
        let start = RouteFixture::sidewalk_at(&ctx, vec3(300.0, car_road_y, 0.0));
        let h = &mut ctx.g.world.humans[human];
        h.location = Location::Outside;
        h.trans.pos = start;
        h.router = Router::new(Some(car));
        (ctx, human, car, destination)
    }

    fn traffic1_plan(ctx: &mut TestCtx, human: HumanID, destination: Vec3) -> Vec<RoutingStep> {
        let (world, resources) = ctx.g.world_res();
        world.humans[human]
            .router
            .go_to(Destination::Outside(destination));
        routing_changed(world, resources, |_, _| None);
        std::mem::take(&mut world.humans[human].router.steps)
    }

    #[test]
    fn traffic1_car_on_a_disconnected_road_is_left_parked() {
        let (mut ctx, human, car, destination) = traffic1_owner_with_car_on(100.0, 400.0);
        let reserved = Bincode::encode(&*ctx.g.read::<ParkingManagement>()).unwrap();
        let steps = traffic1_plan(&mut ctx, human, destination);
        assert!(
            matches!(steps.as_slice(), [RoutingStep::WalkTo(end)] if *end == destination),
            "{steps:?}"
        );
        assert!(ctx.g.world.humans[human].router.last_error.is_none());
        // The parking spot reserved near the destination was released.
        assert_eq!(
            Bincode::encode(&*ctx.g.read::<ParkingManagement>()).unwrap(),
            reserved
        );
        for _ in 0..50 {
            ctx.tick_fast();
        }
        assert!(ctx.g.world.vehicles[car].vehicle.state.is_parked());
    }

    fn traffic1_stranded_driver() -> (TestCtx, HumanID, VehicleID, Vec3) {
        let (mut ctx, human, car, destination) = traffic1_owner_with_car_on(100.0, 400.0);
        crate::transportation::unpark(&mut ctx.g, car);
        let h = &mut ctx.g.world.humans[human];
        h.location = Location::Vehicle(car);
        if let Some(collider) = h.collider.take() {
            ctx.g.write::<TransportGrid>().remove_maintain(collider.0);
        }
        (ctx, human, car, destination)
    }

    fn assert_parks_and_walks(steps: &[RoutingStep], car: VehicleID, destination: Vec3) {
        assert!(
            matches!(steps, [RoutingStep::WalkTo(end), RoutingStep::GetOutVehicle(out),
                RoutingStep::Park(parked, Some(_))]
                if *end == destination && *out == car && *parked == car),
            "{steps:?}"
        );
    }

    #[test]
    fn traffic1_driver_on_a_disconnected_road_parks_and_walks() {
        let (mut ctx, human, car, destination) = traffic1_stranded_driver();
        let (world, resources) = ctx.g.world_res();
        world.humans[human]
            .router
            .go_to(Destination::Outside(destination));
        routing_changed(world, resources, |_, _| None);
        assert_parks_and_walks(&world.humans[human].router.steps, car, destination);
        // Later trips may use the car again, but it must not stay stranded with its driver.
        let settled = (0..3000).any(|_| {
            ctx.tick_fast();
            ctx.g.world.vehicles[car].vehicle.state.is_parked()
                && ctx.g.world.humans[human].location != Location::Vehicle(car)
        });
        assert!(settled);
    }

    #[test]
    fn traffic1_replanning_while_parking_unparks_before_driving() {
        let (mut ctx, human, car, _) = traffic1_owner_with_car_on(100.0, 100.0);
        crate::transportation::unpark(&mut ctx.g, car);
        let spot = {
            let map = ctx.g.map();
            ctx.g
                .write::<ParkingManagement>()
                .reserve_near(vec3(600.0, 100.0, 0.0), &map)
                .unwrap()
        };
        let spot_trans = spot.get(&ctx.g.map().parking).unwrap().trans;
        ctx.g.world.vehicles[car].begin_parking(spot_trans, spot);
        let h = &mut ctx.g.world.humans[human];
        h.location = Location::Vehicle(car);
        if let Some(collider) = h.collider.take() {
            ctx.g.write::<TransportGrid>().remove_maintain(collider.0);
        }
        let destination = RouteFixture::sidewalk_at(&ctx, vec3(800.0, 100.0, 0.0));
        let steps = traffic1_plan(&mut ctx, human, destination);
        assert!(
            matches!(steps.as_slice(), [.., RoutingStep::DriveTo(driven, _), RoutingStep::Unpark(unparked)]
                if *driven == car && *unparked == car),
            "{steps:?}"
        );
        let h = &mut ctx.g.world.humans[human];
        h.router.steps = steps;
        h.router.cur_dest = h.router.target_dest;
        let drove = (0..1000).any(|_| {
            ctx.tick_fast();
            let v = &ctx.g.world.vehicles[car];
            v.collider.is_some() && v.speed.0 > 1.0
        });
        assert!(drove);
    }

    #[test]
    fn traffic1_driver_waiting_for_an_impossible_route_replans() {
        let (mut ctx, human, car, destination) = traffic1_stranded_driver();
        let park_pos = destination - Vec3::Y * 6.0;
        ctx.g.world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Vehicle, park_pos);
        ctx.g.world.humans[human].router.set_test_route(
            Destination::Outside(destination),
            vec![
                RoutingStep::DriveTo(car, park_pos),
                RoutingStep::Park(car, None),
                RoutingStep::GetOutVehicle(car),
                RoutingStep::WalkTo(destination),
            ],
        );
        let (world, resources) = ctx.g.world_res();
        routing_update_system(world, resources);
        routing_changed(world, resources, |_, _| None);
        let steps = std::mem::take(&mut world.humans[human].router.steps);
        assert_parks_and_walks(&steps, car, destination);
    }

    #[test]
    fn traffic1_car_on_a_connected_road_still_drives() {
        let (mut ctx, human, car, destination) = traffic1_owner_with_car_on(100.0, 100.0);
        let steps = traffic1_plan(&mut ctx, human, destination + Vec3::X * 300.0);
        assert!(steps
            .iter()
            .any(|step| matches!(step, RoutingStep::DriveTo(vehicle, _) if *vehicle == car)));
    }

    fn parkfix_tick(ctx: &mut TestCtx, human: HumanID) {
        // Keep decisions from planning a new trip in the middle of the test.
        ctx.g.world.humans[human].decision.wait = u8::MAX;
        ctx.tick_fast();
    }

    /// An owner driving their personal car along a connected road.
    fn parkfix_driving_owner() -> (TestCtx, HumanID, VehicleID) {
        let (mut ctx, human, car, _) = traffic1_owner_with_car_on(100.0, 100.0);
        crate::transportation::unpark(&mut ctx.g, car);
        let h = &mut ctx.g.world.humans[human];
        h.location = Location::Vehicle(car);
        if let Some(collider) = h.collider.take() {
            ctx.g.write::<TransportGrid>().remove_maintain(collider.0);
        }
        let destination = RouteFixture::sidewalk_at(&ctx, vec3(850.0, 100.0, 0.0));
        ctx.g.world.humans[human]
            .router
            .go_to(Destination::Outside(destination));
        let driving = (0..3000).any(|_| {
            parkfix_tick(&mut ctx, human);
            ctx.g.world.humans[human].location == Location::Vehicle(car)
                && ctx.g.world.vehicles[car].speed.0 > 1.0
        });
        let h = &ctx.g.world.humans[human];
        assert!(
            driving,
            "the owner never drove off: {:?} {:?} {:?} {:?} car {:?} {:?}",
            h.location,
            h.router.cur_step(),
            h.router.steps,
            h.router.last_error,
            ctx.g.world.vehicles[car].vehicle.state,
            ctx.g.world.vehicles[car].speed.0
        );
        (ctx, human, car)
    }

    /// The car ends up parked with its owner out of it, and nothing is left on the lane.
    fn parkfix_assert_parked(ctx: &mut TestCtx, human: HumanID, car: VehicleID) {
        let parked = (0..1500).any(|_| {
            parkfix_tick(ctx, human);
            ctx.g.world.vehicles[car].vehicle.state.is_parked()
                && ctx.g.world.humans[human].location != Location::Vehicle(car)
        });
        let v = &ctx.g.world.vehicles[car];
        assert!(
            parked,
            "the car is still on the road: {:?} at {:?}, owner {:?}",
            v.vehicle.state, v.trans.pos, ctx.g.world.humans[human].location
        );
        assert!(v.collider.is_none());
        assert!(abandoned_cars(&ctx.g.world).is_empty());
    }

    #[test]
    fn parkfix_cancelling_a_drive_parks_the_car_where_it_stands() {
        let (mut ctx, human, car) = parkfix_driving_owner();
        let start = ctx.g.world.vehicles[car].trans.pos;
        let (world, resources) = ctx.g.world_res();
        let h = &mut world.humans[human];
        h.router
            .cancel_route(&mut resources.write::<ParkingManagement>());
        h.it = Itinerary::NONE;
        parkfix_assert_parked(&mut ctx, human, car);
        let parked_at = ctx.g.world.vehicles[car].trans.pos;
        assert!(
            parked_at.distance(start) <= NEAR_PARKING_RADIUS + 10.0,
            "parked at {parked_at:?}, the drive was cancelled at {start:?}"
        );
    }

    #[test]
    fn parkfix_walking_on_from_a_moving_car_parks_it_first() {
        let (mut ctx, human, car) = parkfix_driving_owner();
        let destination = RouteFixture::sidewalk_at(&ctx, vec3(500.0, 100.0, 0.0));
        let (world, resources) = ctx.g.world_res();
        let router = &mut world.humans[human].router;
        router.walk_only_pickup = true;
        router.go_to(Destination::Outside(destination));
        routing_changed(world, resources, |_, _| None);
        assert_parks_and_walks(&world.humans[human].router.steps, car, destination);
        parkfix_assert_parked(&mut ctx, human, car);
    }

    #[test]
    fn parkfix_aborting_a_drive_parks_the_car_before_the_owner_leaves() {
        let (mut ctx, human, car) = parkfix_driving_owner();
        let (world, resources) = ctx.g.world_res();
        let map = resources.read::<Map>();
        let h = &mut world.humans[human];
        h.router
            .cancel_route(&mut resources.write::<ParkingManagement>());
        h.it = Itinerary::NONE;
        Router::exit_aborted_vehicle(h, car, &world.vehicles, &map, resources);
        drop(map);
        parkfix_assert_parked(&mut ctx, human, car);
    }

    /// A car left driving with no route and nobody inside, as older saves may contain.
    fn parkfix_abandoned_car(at_x: f32) -> (TestCtx, HumanID, VehicleID) {
        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(1500.0, 100.0, 0.0)]);
        let house = ctx.build_house_near(vec2(300.0, 100.0));
        let human = spawn_human(&mut ctx.g, house).unwrap();
        let pos = {
            let map = ctx.g.map();
            let pos = vec3(at_x, 100.0, 0.0);
            map.get(map.nearest_lane(pos, LaneKind::Driving, None).unwrap())
                .unwrap()
                .points
                .project_segment_dir(pos)
        };
        let car = spawn_driving_vehicle(
            &mut ctx.g,
            VehicleKind::Car,
            Transform {
                pos: pos.0,
                dir: pos.2,
            },
        )
        .unwrap();
        ctx.g.world.humans[human].router = Router::new(Some(car));
        assert_eq!(abandoned_cars(&ctx.g.world), vec![(car, human)]);
        (ctx, human, car)
    }

    #[test]
    fn parkfix_an_abandoned_car_is_parked_near_where_it_stands() {
        let (mut ctx, human, car) = parkfix_abandoned_car(900.0);
        let start = ctx.g.world.vehicles[car].trans.pos;
        parkfix_assert_parked(&mut ctx, human, car);
        assert!(ctx.g.world.vehicles[car].trans.pos.distance(start) <= NEAR_PARKING_RADIUS + 10.0);
    }

    #[test]
    fn parkfix_an_abandoned_car_without_a_spot_nearby_is_parked_at_home() {
        let (mut ctx, human, car) = parkfix_abandoned_car(1200.0);
        let start = ctx.g.world.vehicles[car].trans.pos;
        {
            let map = ctx.g.map();
            let mut parking = ctx.g.write::<ParkingManagement>();
            while parking
                .reserve_near_radius(start, &map, NEAR_PARKING_RADIUS)
                .is_ok()
            {}
        }
        parkfix_assert_parked(&mut ctx, human, car);
        let home = ctx.g.world.humans[human].home.house;
        let door = ctx.g.map().get(home).unwrap().door_pos;
        let parked_at = ctx.g.world.vehicles[car].trans.pos;
        assert!(
            parked_at.distance(door) < parked_at.distance(start),
            "parked at {parked_at:?}, home {door:?}, abandoned at {start:?}"
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
    fn transitfix_carless_household_errands_may_ride() {
        let errands: [fn(&mut HumanEnt); 3] = [
            |h| h.household.basket = Some(BasketId(1)),
            |h| h.household.leisure_visit = Some(h.home.house),
            |h| {
                h.household.carried.insert(ItemID::new("bread"), 1);
            },
        ];
        for errand in errands {
            let mut f = RouteFixture::new(500.0);
            let human = &mut f.ctx.g.world.humans[f.human];
            human.router.walk_only_pickup = true;
            errand(human);
            f.assert_ride(400);

            // A car owner's errand stays on foot, without the car.
            let mut f = RouteFixture::new(500.0);
            f.assign_car(f.origin + Vec3::X * 401.0);
            let human = &mut f.ctx.g.world.humans[f.human];
            human.router.walk_only_pickup = true;
            errand(human);
            let destination = f.destination;
            assert!(matches!(f.plan(400), [RoutingStep::WalkTo(end)] if *end == destination));
        }
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
                load: None,
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
    fn p3b_transit_must_beat_walking_by_the_savings_margin() {
        let mut tie = RouteFixture::new(240.0);
        let destination = tie.destination;
        // Walking is about 10,000 ticks. Riding takes 1,500 ticks plus half the headway.
        assert!(matches!(tie.plan(17_000), [RoutingStep::WalkTo(end)] if *end == destination));
        let mut near_tie = RouteFixture::new(240.0);
        assert!(matches!(near_tie.plan(15_600), [RoutingStep::WalkTo(end)] if *end == destination));
        RouteFixture::new(240.0).assert_ride(15_599);
    }

    #[test]
    fn transitfix_carless_walk_is_costed_on_the_sidewalk_route() {
        // A U-shaped street: the far side is 200 m across but about 1.6 km on foot.
        let detour = || {
            let ctx = TestCtx::new();
            ctx.build_roads(&[
                vec3(100.0, 100.0, 0.0),
                vec3(900.0, 100.0, 0.0),
                vec3(900.0, 300.0, 0.0),
                vec3(100.0, 300.0, 0.0),
            ]);
            RouteFixture::serving(ctx, |ctx, _| {
                RouteFixture::sidewalk_at(ctx, vec3(200.0, 300.0, 0.0))
            })
        };
        let mut f = detour();
        let walk = Itinerary::route(
            Tick(0),
            f.origin,
            f.destination,
            &f.ctx.g.map(),
            PathKind::Pedestrian,
        )
        .and_then(|it| transit_path::walking_distance(&it, f.origin, f.destination, &f.ctx.g.map()))
        .unwrap();
        let across = f.origin.distance(f.destination);
        assert!(walk > 5.0 * across, "walk {walk} m, across {across} m");
        // Riding beats the straight line only with a short wait, and the sidewalk route
        // even with a long one.
        let walk_ticks = f64::from(across) / 1.2 * prototypes::TICKS_PER_REALTIME_SECOND as f64;
        let headway = (2.0 * walk_ticks) as u64;
        f.assert_ride(headway);
        let mut close = RouteFixture::new(across);
        let destination = close.destination;
        assert!(matches!(close.plan(headway), [RoutingStep::WalkTo(end)] if *end == destination));
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
        // Walk along the sidewalk's direction; the ride must go round by the far stop.
        let [board, alight] = f.stops;
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

    #[test]
    fn p3b_carless_commute_does_not_chase_a_departing_bus() {
        let trip = |with_service| {
            let mut ctx = TestCtx::new();
            ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(900.0, 100.0, 0.0)]);
            let house = ctx.build_house_near(vec2(200.0, 100.0));
            let human = spawn_human(&mut ctx.g, house).unwrap();
            let stops = {
                let map = ctx.g.map();
                let lane = map
                    .lanes()
                    .values()
                    .find(|lane| {
                        lane.kind == LaneKind::Driving
                            && lane.points.first().x < lane.points.last().x
                    })
                    .unwrap();
                [200.0, 390.0].map(|x| {
                    let (pos, _, dir) = lane.points.project_segment_dir(vec3(x, 100.0, 0.0));
                    WorldCommand::TransitAddStop { pos, dir: dir.xy() }
                })
            };
            ctx.apply(&stops);
            let stops: Vec<_> = ctx.g.read::<Transit>().stops.keys().collect();
            let (board, destination) = {
                let transit = ctx.g.read::<Transit>();
                (
                    transit.stops[stops[0]].walk_pos,
                    transit.stops[stops[1]].walk_pos,
                )
            };
            let origin = board - Vec3::X * 1.2;
            ctx.apply(&[WorldCommand::TransitCreateLine { stops }]);
            let line = ctx.g.read::<Transit>().lines.keys().next().unwrap();
            ctx.apply(&[WorldCommand::TransitSetVehicles {
                line,
                n: u8::from(with_service),
            }]);
            parkfix_tick(&mut ctx, human);
            let (world, resources) = ctx.g.world_res();
            let now = elapsed_ticks(resources);
            if with_service {
                let mut transit = resources.write::<Transit>();
                // At 1.2 m/s the stop is 50 ticks away at full speed. The
                // pedestrian starting from rest needs longer than 52 ticks.
                transit.runs.values_mut().next().unwrap().dwell_until = Some(Tick(now.0 + 52));
            }
            world.edit_human(human, |h| {
                h.location = Location::Outside;
                h.trans.pos = origin;
                h.speed.0 = 0.0;
                h.it = Itinerary::NONE;
                h.router = Router::new(None);
                h.pedestrian.walking_speed = 1.2;
                h.router.go_to(Destination::Outside(destination));
            });
            routing_changed_system(world, resources);
            let mode = world.humans[human].router.trip_mode.mode();
            let duration = (1..=crate::map_dynamic::transit::GIVE_UP_TICKS + 10_000)
                .find(|_| {
                    parkfix_tick(&mut ctx, human);
                    ctx.g.world.humans[human].router.trip_start.is_none()
                })
                .unwrap_or_else(|| panic!("{mode:?} trip did not finish"));
            assert!(ctx.g.world.humans[human]
                .trans
                .pos
                .is_close(destination, 1.0));
            (mode, duration)
        };

        let (_, walk_ticks) = trip(false);
        let (mode, with_service_ticks) = trip(true);
        assert!(
            with_service_ticks <= walk_ticks,
            "{mode:?} took {with_service_ticks} ticks, walking took {walk_ticks}"
        );
        assert_eq!(mode, TripMode::Walk);
    }

    fn finalsim_fill_spots_near(ctx: &TestCtx, pos: Vec3) {
        let map = ctx.g.map();
        let mut parking = ctx.g.write::<ParkingManagement>();
        while parking
            .reserve_near_radius(pos, &map, NEAR_PARKING_RADIUS)
            .is_ok()
        {}
    }

    fn finalsim_road_of_spot(map: &Map, spot: &SpotReservation) -> Option<crate::map::RoadID> {
        let spot = spot.get(&map.parking)?;
        Some(map.get(spot.parent)?.parent)
    }

    #[test]
    fn finalsim_a_stuck_park_plan_without_a_destination_is_dropped() {
        let (mut ctx, human, car, destination) = traffic1_stranded_driver();
        let park_pos = destination - Vec3::Y * 6.0;
        ctx.g.world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Vehicle, park_pos);
        {
            let router = &mut ctx.g.world.humans[human].router;
            router.set_test_route(
                Destination::Outside(destination),
                vec![
                    RoutingStep::DriveTo(car, park_pos),
                    RoutingStep::Park(car, None),
                    RoutingStep::GetOutVehicle(car),
                ],
            );
            // Park plans run after the trip was stopped.
            router.target_dest = None;
            router.cur_dest = None;
        }
        let (world, resources) = ctx.g.world_res();
        routing_update_system(world, resources);
        assert!(
            !matches!(
                world.humans[human].router.cur_step,
                Some(RoutingStep::DriveTo(..))
            ),
            "the drive that cannot reach its spot is dropped"
        );
        parkfix_assert_parked(&mut ctx, human, car);
    }

    #[test]
    fn finalsim_an_aborted_drive_without_a_spot_beside_the_car_lets_the_owner_out() {
        let (mut ctx, human, car) = parkfix_driving_owner();
        let at = ctx.g.world.vehicles[car].trans.pos;
        finalsim_fill_spots_near(&ctx, at);
        let (world, resources) = ctx.g.world_res();
        let map = resources.read::<Map>();
        let h = &mut world.humans[human];
        h.router
            .cancel_route(&mut resources.write::<ParkingManagement>());
        h.it = Itinerary::NONE;
        Router::exit_aborted_vehicle(h, car, &world.vehicles, &map, resources);
        drop(map);
        let h = &world.humans[human];
        assert_eq!(h.location, Location::Outside, "{:?}", h.router.steps);
        assert!(!h.router.plans_with(car), "{:?}", h.router.steps);
        for _ in 0..5 {
            parkfix_tick(&mut ctx, human);
        }
        assert!(abandoned_cars(&ctx.g.world).is_empty());
        assert!(ctx
            .g
            .world
            .vehicles
            .get(car)
            .is_none_or(|v| on_road(v).is_none()));
    }

    #[test]
    fn finalsim_an_idle_car_without_a_spot_beside_it_lets_the_owner_out_on_the_sidewalk() {
        let (mut ctx, human, car) = parkfix_driving_owner();
        let at = ctx.g.world.vehicles[car].trans.pos;
        finalsim_fill_spots_near(&ctx, at);
        {
            let (world, resources) = ctx.g.world_res();
            let h = &mut world.humans[human];
            h.router
                .cancel_route(&mut resources.write::<ParkingManagement>());
            h.it = Itinerary::NONE;
        }
        parkfix_tick(&mut ctx, human);
        parkfix_tick(&mut ctx, human);
        let h = &ctx.g.world.humans[human];
        assert_eq!(h.location, Location::Outside, "{:?}", h.router.steps);
        let map = ctx.g.map();
        let sidewalk = map
            .get(
                map.nearest_lane(h.trans.pos, LaneKind::Walking, None)
                    .unwrap(),
            )
            .unwrap()
            .points
            .project(h.trans.pos);
        assert!(
            sidewalk.distance(h.trans.pos) < 0.5,
            "the owner stands at {:?}, the sidewalk is at {sidewalk:?}",
            h.trans.pos
        );
    }

    #[test]
    fn finalsim_a_car_parks_in_place_only_beside_its_own_road() {
        let ctx = TestCtx::new();
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(900.0, 100.0, 0.0)]);
        ctx.build_roads(&[vec3(500.0, 100.0, 0.0), vec3(500.0, 400.0, 0.0)]);
        let pos = {
            let map = ctx.g.map();
            let pos = vec3(470.0, 100.0, 0.0);
            map.get(map.nearest_lane(pos, LaneKind::Driving, None).unwrap())
                .unwrap()
                .points
                .project(pos)
        };
        let map = ctx.g.map();
        let own = map
            .get(PathKind::Vehicle.nearest_lane(&map, pos).unwrap())
            .unwrap()
            .parent;
        let mut parking = ctx.g.write::<ParkingManagement>();
        // Fill the spots beside the car's road that an in-place search finds first.
        loop {
            let spot = parking
                .reserve_near_radius(pos, &map, NEAR_PARKING_RADIUS)
                .unwrap();
            if finalsim_road_of_spot(&map, &spot) != Some(own) {
                parking.free(spot);
                break;
            }
            std::mem::forget(spot);
        }
        let car = VehicleID::default();
        let steps = park_where_it_stands(car, pos, &mut parking, &map).unwrap_or_default();
        if let Some(RoutingStep::Park(_, Some(spot))) = steps.first() {
            assert_eq!(
                finalsim_road_of_spot(&map, spot),
                Some(own),
                "a car only glides into a spot beside its own road: {steps:?}"
            );
        }
    }

    #[test]
    fn finalsim_an_owner_in_a_parked_car_walks_to_an_unreachable_destination() {
        let (mut ctx, human, car, destination) = traffic1_owner_with_car_on(100.0, 400.0);
        let h = &mut ctx.g.world.humans[human];
        h.location = Location::Vehicle(car);
        if let Some(collider) = h.collider.take() {
            ctx.g.write::<TransportGrid>().remove_maintain(collider.0);
        }
        assert!(ctx.g.world.vehicles[car].vehicle.state.is_parked());
        let reserved = Bincode::encode(&*ctx.g.read::<ParkingManagement>()).unwrap();
        let steps = traffic1_plan(&mut ctx, human, destination);
        assert!(
            matches!(steps.as_slice(), [RoutingStep::WalkTo(end), RoutingStep::GetOutVehicle(out)]
                if *end == destination && *out == car),
            "{steps:?}"
        );
        assert_eq!(
            Bincode::encode(&*ctx.g.read::<ParkingManagement>()).unwrap(),
            reserved,
            "the spot reserved near the destination was released"
        );
    }

    #[test]
    fn perf6_a_waiting_driver_checks_its_road_again_only_after_a_lane_change() {
        let (mut ctx, human, car, destination) = traffic1_stranded_driver();
        // A connector between both roads makes the destination reachable by car.
        ctx.build_roads(&[vec3(900.0, 100.0, 0.0), vec3(900.0, 400.0, 0.0)]);
        let connector = ctx
            .g
            .map()
            .roads()
            .iter()
            .find(|(_, road)| {
                [road.points.first(), road.points.last()]
                    == [vec3(900.0, 100.0, 0.3), vec3(900.0, 400.0, 0.3)]
            })
            .map(|(id, _)| id)
            .unwrap();
        let park_pos = destination - Vec3::Y * 6.0;
        ctx.g.world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Vehicle, park_pos);
        ctx.g.world.humans[human].router.set_test_route(
            Destination::Outside(destination),
            vec![
                RoutingStep::DriveTo(car, park_pos),
                RoutingStep::Park(car, None),
                RoutingStep::GetOutVehicle(car),
                RoutingStep::WalkTo(destination),
            ],
        );
        // The trip is planned again from scratch once its destination is reset.
        let driving = |ctx: &TestCtx| {
            let router = &ctx.g.world.humans[human].router;
            matches!(router.cur_step, Some(RoutingStep::DriveTo(..))) && router.cur_dest.is_some()
        };

        CAN_DRIVE_CHECKS.set(0);
        for _ in 0..5 {
            let (world, resources) = ctx.g.world_res();
            routing_update_system(world, resources);
            assert!(driving(&ctx), "the car can still reach its spot");
        }
        assert_eq!(
            CAN_DRIVE_CHECKS.get(),
            1,
            "the unchanged road is checked once"
        );

        ctx.g.map_mut().remove_road(connector);
        let (world, resources) = ctx.g.world_res();
        routing_update_system(world, resources);
        assert_eq!(
            CAN_DRIVE_CHECKS.get(),
            2,
            "a lane change checks the road again"
        );
        assert!(
            !driving(&ctx),
            "the drive that lost its road is planned again"
        );
        let (world, resources) = ctx.g.world_res();
        routing_changed(world, resources, |_, _| None);
        let steps = std::mem::take(&mut world.humans[human].router.steps);
        assert_parks_and_walks(&steps, car, destination);
    }
}

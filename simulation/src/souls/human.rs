use crate::economy::cargo::{deliver_load, LoadId};
use crate::economy::{BasketFailure, Bought, DailyEconomyMetrics, Market, TripPurpose};
use crate::map::BuildingID;
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{
    BuildingInfos, Destination, ElectricityFlow, Itinerary, ParkingManagement, Router,
    WalkingReach, NEAR_PARKING_RADIUS,
};
use crate::souls::desire::{
    leisure_score, BuyFood, BuyGoods, Home, ParkChoices, Work, WorkKind, AFTER_WORK_RADIUS,
    PARK_DWELL, PARK_TRAVEL_LIMIT,
};
use crate::souls::household::{
    cancel_visit, household_items, BasketId, HouseholdClaims, BASKET_TRAVEL_LIMIT, HOMEBOUND_RETRY,
    HOMEBOUND_TRAVEL_LIMIT,
};
use crate::souls::residence::Residences;
use crate::transportation::Speed;
use crate::transportation::{
    random_pedestrian_shirt_color, spawn_parked_vehicle, Location, Pedestrian, VehicleKind,
};
use crate::utils::rand_provider::RandProvider;
use crate::utils::resources::Resources;
use crate::world::{CompanyID, FreightStationEnt, HumanEnt, HumanID, VehicleEnt, VehicleID};
use crate::{BuildingKind, Map, ParCommandBuffer, Simulation, SoulID, World};
use egui_inspect::Inspect;
use geom::{Transform, Vec3};
use lazy_static::lazy_static;
use prototypes::{GameTime, ItemID, TICKS_PER_HOUR, TICKS_PER_MINUTE, TICKS_PER_REALTIME_SECOND};
use serde::{Deserialize, Serialize};
use slotmapd::{HopSlotMap, Key};

/// A trip home waiting this long for a route has stalled (movement, real time).
pub(crate) const HOMEBOUND_STALL: u64 = 30 * TICKS_PER_REALTIME_SECOND;

#[derive(Inspect, Serialize, Deserialize, Default)]
pub struct HumanDecision {
    pub kind: HumanDecisionKind,
    pub wait: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HumanDecisionKind {
    Yield,
    SetVehicle(Option<VehicleID>),
    GoTo(Destination),
    DeliverAtBuilding(BuildingID),
    MultiStack(Vec<HumanDecisionKind>),
    PickupBasket(BasketId),
    DepositAtHome,
    VisitLeisure(BuildingID),
    DeliverLoad {
        destination: BuildingID,
        load: LoadId,
    },
    GoHome(BuildingID),
    GoWork {
        workplace: BuildingID,
        employer: Option<CompanyID>,
    },
}

#[derive(Debug, Serialize, Deserialize, Inspect)]
pub enum Gender {
    M,
    F,
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct PersonalInfo {
    pub name: String,
    pub age: u8,
    pub gender: Gender,
}

debug_inspect_impl!(HumanDecisionKind);

static FIRST_NAMES_BYTES: &str = include_str!("first_names.txt");
static LAST_NAMES_BYTES: &str = include_str!("names.txt");

lazy_static! {
    static ref LAST_NAMES: Vec<&'static str> = LAST_NAMES_BYTES.split('\n').collect();
    static ref FIRST_NAMES: Vec<&'static str> = FIRST_NAMES_BYTES.split('\n').collect();
}

impl PersonalInfo {
    pub fn new(rng: &mut RandProvider) -> Self {
        let age = (rng.next_f32() * 30.0 + 20.0) as u8;
        let gender = match rng.next_u32() % 2 {
            0 => Gender::M,
            1 => Gender::F,
            _ => unreachable!(),
        };

        let first_name = FIRST_NAMES[rng.next_u32() as usize % FIRST_NAMES.len()];
        let last_name = LAST_NAMES[rng.next_u32() as usize % LAST_NAMES.len()];

        let name = format!("{} {}", first_name, last_name);

        Self { name, age, gender }
    }
}

impl Default for HumanDecisionKind {
    fn default() -> Self {
        Self::Yield
    }
}

impl HumanDecisionKind {
    pub(crate) fn references_any_building(
        &self,
        includes: impl Fn(BuildingID) -> bool + Copy,
    ) -> bool {
        match self {
            Self::GoTo(Destination::Building(id))
            | Self::GoHome(id)
            | Self::GoWork { workplace: id, .. }
            | Self::DeliverAtBuilding(id)
            | Self::DeliverLoad {
                destination: id, ..
            }
            | Self::VisitLeisure(id) => includes(*id),
            Self::MultiStack(steps) => steps
                .iter()
                .any(|step| step.references_any_building(includes)),
            _ => false,
        }
    }

    pub(crate) fn references_employer(&self, employer: CompanyID) -> bool {
        match self {
            Self::GoWork {
                employer: Some(id), ..
            } => *id == employer,
            Self::MultiStack(steps) => steps.iter().any(|step| step.references_employer(employer)),
            _ => false,
        }
    }

    pub(crate) fn references_vehicle(&self, vehicle: VehicleID) -> bool {
        match self {
            Self::SetVehicle(Some(id)) => *id == vehicle,
            Self::MultiStack(steps) => steps.iter().any(|step| step.references_vehicle(vehicle)),
            _ => false,
        }
    }

    pub(crate) fn references_load(&self, token: LoadId) -> bool {
        match self {
            Self::DeliverLoad { load, .. } => *load == token,
            Self::MultiStack(steps) => steps.iter().any(|step| step.references_load(token)),
            _ => false,
        }
    }

    pub fn update(
        &mut self,
        router: &mut Router,
        binfos: &BuildingInfos,
        map: &Map,
        cbuf_freight: &ParCommandBuffer<FreightStationEnt>,
        cbuf: &ParCommandBuffer<HumanEnt>,
        driver: HumanID,
        work: Option<&mut Work>,
    ) -> bool {
        match *self {
            HumanDecisionKind::GoTo(dest) => {
                let purpose = trip_purpose(dest, work.as_deref(), router);
                router.go_to_for(dest, purpose)
            }
            HumanDecisionKind::GoHome(home) => router.go_home(home),
            HumanDecisionKind::GoWork {
                workplace,
                employer,
            } => {
                if work.is_some_and(|work| work.workplace == workplace && work.employer == employer)
                {
                    router.go_to_work(workplace, employer)
                } else {
                    true
                }
            }
            HumanDecisionKind::MultiStack(ref mut decisions) => {
                if let Some(d) = decisions.last_mut() {
                    if d.update(router, binfos, map, cbuf_freight, cbuf, driver, work) {
                        decisions.pop();
                    }
                    false
                } else {
                    true
                }
            }
            HumanDecisionKind::SetVehicle(id) => {
                router.use_vehicle(id);
                true
            }
            HumanDecisionKind::DeliverLoad { destination, load } => {
                // The unload validates the whole trip. A refusal cancels the
                // load with its reason instead of retrying the drive.
                if let Some(WorkKind::Driver { truck, .. }) = work.as_deref().map(|work| work.kind)
                {
                    cbuf.exec_ent(driver, move |sim| {
                        deliver_load(sim, load, driver, truck, destination);
                    });
                }
                true
            }
            HumanDecisionKind::DeliverAtBuilding(bid) => {
                if let Some(WorkKind::Driver { deliver_order, .. }) =
                    work.map(|work| &mut work.kind)
                {
                    if *deliver_order == Some(bid) {
                        *deliver_order = None;
                    }
                }
                let Some(b) = map.buildings().get(bid) else {
                    return true;
                };
                if matches!(b.kind, BuildingKind::RailFreightStation(_)) {
                    let Some(SoulID::FreightStation(fid)) = binfos.owner(bid) else {
                        return true;
                    };
                    cbuf_freight.exec_ent(fid, move |e| {
                        let (world, resources) = e.world_res();
                        if let Some(f) = world.freight_stations.get_mut(fid) {
                            let terminal = crate::economy::cargo::Endpoint {
                                soul: SoulID::FreightStation(fid),
                                building: bid,
                            };
                            if resources
                                .read::<crate::economy::cargo::FreightTrace>()
                                .export_units_fit(terminal, f.f.waiting_cargo, 1)
                            {
                                f.f.accept_export(1);
                            }
                        }
                    });
                }
                true
            }
            HumanDecisionKind::Yield => true,
            HumanDecisionKind::PickupBasket(_)
            | HumanDecisionKind::DepositAtHome
            | HumanDecisionKind::VisitLeisure(_) => {
                debug_assert!(false, "household actions require the resident scheduler");
                true
            }
        }
    }
}

impl HumanDecision {
    /// Number of simulation steps before the next decision, independent of calendar jumps.
    #[inline]
    fn wait_one_tick(&mut self) -> bool {
        if let Some(wait) = self.wait.checked_sub(1) {
            self.wait = wait;
            return true;
        }
        false
    }
}

fn trip_purpose(dest: Destination, work: Option<&Work>, router: &Router) -> TripPurpose {
    if router.walk_only_pickup {
        return TripPurpose::Shopping;
    }
    let Destination::Building(building) = dest else {
        return TripPurpose::Other;
    };
    if let Some(work) = work {
        if let WorkKind::Driver { truck, .. } = work.kind {
            if router.selected_vehicle() == Some(truck) {
                return if building == work.workplace {
                    TripPurpose::FreightReturn
                } else {
                    TripPurpose::FreightOutbound
                };
            }
        }
    }
    TripPurpose::Other
}

impl HumanEnt {
    pub(crate) fn attend_work(&mut self, time: &GameTime, building: BuildingID) {
        if self.work.as_ref().is_some_and(|work| {
            work.workplace == building && work.work_inter.dist_start(&time.daytime) == 0
        }) {
            self.household.attended_shift = Some((building, time.daytime.day));
        }
    }

    pub(crate) fn attend_work_for(
        &mut self,
        time: &GameTime,
        building: BuildingID,
        infos: &BuildingInfos,
    ) {
        let Some(work) = self.work.as_mut().filter(|work| work.workplace == building) else {
            return;
        };
        if let Some(SoulID::GoodsCompany(employer)) = infos.owner(building) {
            work.record_attendance(time, employer);
            if work.work_inter.is_active(&time.daytime)
                && work.employer.is_none_or(|assigned| assigned == employer)
            {
                self.router.record_work_stay(employer);
            }
        }
        if work.employer.is_none() {
            self.attend_work(time, building);
        }
    }

    fn attended_work_here(&self, time: &GameTime, infos: &BuildingInfos) -> bool {
        self.work.as_ref().is_some_and(|work| {
            self.location == Location::Building(work.workplace)
                && (work.employer.is_none()
                    || work.attended_today(
                        time,
                        match infos.owner(work.workplace) {
                            Some(SoulID::GoodsCompany(employer)) => Some(employer),
                            _ => None,
                        },
                    ))
        })
    }

    pub(crate) fn go_home(&self) -> HumanDecisionKind {
        if self.location == Location::Building(self.home.house) {
            HumanDecisionKind::Yield
        } else {
            HumanDecisionKind::GoHome(self.home.house)
        }
    }

    fn start_walk(&mut self, destination: BuildingID, purpose: TripPurpose, resources: &Resources) {
        // At the end of a commute there may still be a finished building-entry step.
        self.stop_route(resources);
        self.router.walk_only_pickup = true;
        self.router
            .go_to_for(Destination::Building(destination), purpose);
    }

    fn stop_route(&mut self, resources: &Resources) {
        if crate::map_dynamic::transit::on_route_cancelled(self, resources) {
            return;
        }
        self.router
            .cancel_route(&mut resources.write::<ParkingManagement>());
        self.it = Itinerary::NONE;
    }

    fn update_household_action(
        &mut self,
        id: HumanID,
        now: u64,
        parks: &ParkChoices,
        vehicles: &HopSlotMap<VehicleID, VehicleEnt>,
        walking: &mut WalkingReach,
        resources: &Resources,
    ) -> bool {
        let map = parks.map;
        match self.decision.kind {
            HumanDecisionKind::PickupBasket(id) => {
                let Some(basket) = self.household.reservation.as_ref() else {
                    return true;
                };
                let arrived = self.location == Location::Building(basket.building);
                let failure = if parks.infos.owner(basket.building) != Some(basket.seller)
                    || map.get(basket.building).is_none_or(|store| {
                        store.development().is_some_and(|site| {
                            site.business != Some(crate::map::GrowableBusinessState::Open)
                                || store.is_abandoned_growable()
                        })
                    })
                {
                    Some(BasketFailure::SellerRemoved)
                } else if now >= basket.expires {
                    Some(BasketFailure::Expired)
                } else if map
                    .get(basket.building)
                    .is_some_and(|store| store.is_orphaned())
                    || (!arrived
                        && (self.router.last_error.is_some()
                            || self.router.target_dest
                                != Some(Destination::Building(basket.building))
                            || self.it.is_wait_for_reroute().is_some()))
                {
                    Some(BasketFailure::RouteInterrupted)
                } else {
                    None
                };
                if let Some(failure) = failure {
                    self.household.cancel(resources, failure);
                    self.router.walk_only_pickup = false;
                    self.stop_route(resources);
                    true
                } else if arrived {
                    let local = self.home.house == basket.building
                        && parks.infos.residence_of(SoulID::Human(basket.shopper))
                            == Some(basket.building)
                        && map
                            .get(basket.building)
                            .is_some_and(|building| building.is_mixed_use());
                    self.household.collect(id, now, resources);
                    #[cfg(test)]
                    if self.household.reservation.is_none() {
                        if let Ok(mut pickups) = resources.try_write::<Vec<BasketId>>() {
                            pickups.push(id);
                        }
                    }
                    self.router.walk_only_pickup = false;
                    self.stop_route(resources);
                    if local && !self.household.carried.is_empty() {
                        self.household.deposit(now, resources);
                        resources
                            .write::<DailyEconomyMetrics>()
                            .record_local_pickup(now);
                        self.decision.kind = HumanDecisionKind::Yield;
                        return true;
                    }
                    self.decision.kind = HumanDecisionKind::DepositAtHome;
                    false
                } else {
                    false
                }
            }
            HumanDecisionKind::DepositAtHome => {
                let home = self.home.house;
                let dest = Destination::Building(home);
                if self.location == Location::Building(home) {
                    self.router.walk_only_pickup = false;
                    self.household.deposit(now, resources);
                    return true;
                }
                if let Some(until) = self.household.homebound_until {
                    let started = until.saturating_sub(HOMEBOUND_TRAVEL_LIMIT);
                    let waiting_for_route = match self.location {
                        Location::Vehicle(car) => vehicles
                            .get(car)
                            .is_none_or(|car| car.it.is_wait_for_reroute().is_some()),
                        Location::Transit(_) => false,
                        _ => self.it.is_wait_for_reroute().is_some(),
                    };
                    let stalled =
                        waiting_for_route && now.saturating_sub(started) >= HOMEBOUND_STALL;
                    if now >= until
                        || stalled
                        || self.router.last_error.is_some()
                        || self.router.target_dest != Some(dest)
                    {
                        if matches!(self.location, Location::Transit(_)) {
                            self.router.cancel_at_alight();
                        } else {
                            self.stop_route(resources);
                            if let Location::Vehicle(car) = self.location {
                                Router::exit_aborted_vehicle(self, car, vehicles, map, resources);
                            }
                        }
                        self.router.walk_only_pickup = false;
                        self.household.homebound_until = None;
                        self.household.next_deposit =
                            now + HOMEBOUND_RETRY + common::hash_u64(id) % (20 * TICKS_PER_MINUTE);
                        return true;
                    }
                    return false;
                }

                let origin = self.ground_position(map);
                let walk_home = map.get(home).is_some_and(|building| {
                    let car_near_home = self
                        .router
                        .personal_car
                        .and_then(|id| vehicles.get(id))
                        .is_none_or(|car| {
                            car.trans.pos.distance2(building.door_pos)
                                <= NEAR_PARKING_RADIUS.powi(2)
                        });
                    car_near_home
                        && origin.distance2(building.door_pos) <= AFTER_WORK_RADIUS.powi(2)
                        && walking
                            .component_at(map, origin)
                            .is_some_and(|start| walking.reaches_building(map, start, building))
                });
                self.household.homebound_until = Some(now + HOMEBOUND_TRAVEL_LIMIT);
                if walk_home {
                    self.start_walk(home, TripPurpose::Home, resources);
                } else {
                    self.stop_route(resources);
                    self.router.walk_only_pickup = false;
                    self.router.go_to_for(dest, TripPurpose::Home);
                }
                false
            }
            HumanDecisionKind::VisitLeisure(park) => {
                let arrived = self.location == Location::Building(park);
                if !parks.still_has_slot(&resources.read::<HouseholdClaims>(), park, id)
                    || self.router.last_error.is_some()
                    || (!arrived && self.router.target_dest != Some(Destination::Building(park)))
                    || (!arrived && now - self.household.leisure_started >= PARK_TRAVEL_LIMIT)
                    || (!arrived && self.household.leisure_until.is_some())
                {
                    cancel_visit(id, self, resources);
                    self.stop_route(resources);
                    return true;
                }
                if !arrived {
                    return false;
                }
                let until = if let Some(until) = self.household.leisure_until {
                    until
                } else {
                    let mut claims = resources.write::<HouseholdClaims>();
                    claims.remove_park_claim(park, id, &self.household);
                    let until = now + PARK_DWELL;
                    self.household.leisure_until = Some(until);
                    claims.add_park_claim(park, id, &self.household);
                    until
                };
                if now < until {
                    return false;
                }
                self.household.needs.leisure = 1.0;
                self.household.next_leisure = now + 3 * 24 * TICKS_PER_HOUR;
                cancel_visit(id, self, resources);
                self.stop_route(resources);
                true
            }
            _ => true,
        }
    }

    fn ground_position(&self, map: &Map) -> Vec3 {
        match self.location {
            Location::Building(id) => map
                .get(id)
                .map_or(self.trans.pos, |building| building.door_pos),
            _ => self.trans.pos,
        }
    }
}

pub fn update_decision_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("souls::update_decision_system");
    let time = &*resources.read::<GameTime>();
    let infos = &*resources.read::<BuildingInfos>();
    let map = &*resources.read::<Map>();
    let cbuf = &*resources.read::<ParCommandBuffer<HumanEnt>>();
    let freight = &*resources.read::<ParCommandBuffer<FreightStationEnt>>();
    let now = elapsed_ticks(resources).0;
    let vehicles = &world.vehicles;
    let mut walking = resources.write::<WalkingReach>();
    let parks = ParkChoices {
        companies: &world.companies,
        map,
        infos,
        electricity: &resources.read::<ElectricityFlow>(),
        walk_distances: Default::default(),
    };

    world.humans.edit_each_split(|id, h| {
        if h.it.is_pedestrian_egress() {
            return;
        }
        if h.goods.has_pickup() {
            BuyGoods::update_pickup(h, time, map, vehicles, resources);
        }
        if h.decision_mut().wait_one_tick() {
            let pickup_interrupted = matches!(h.decision.kind, HumanDecisionKind::PickupBasket(_))
                && h.household
                    .reservation
                    .as_ref()
                    .is_some_and(|basket| now >= basket.expires || h.router.last_error.is_some());
            if !pickup_interrupted {
                return;
            }
        }
        let h: &mut HumanEnt = h;
        h.household.refresh_needs(
            now,
            h.location == Location::Building(h.home.house),
            resources,
        );
        h.household.expire_order(now, resources);
        let origin = h.ground_position(map);
        h.decision.wait = (30.0 + common::rand::rand2(origin.x, origin.y) * 50.0) as u8;

        if matches!(
            h.decision.kind,
            HumanDecisionKind::PickupBasket(_)
                | HumanDecisionKind::DepositAtHome
                | HumanDecisionKind::VisitLeisure(_)
        ) {
            if !h.update_household_action(id, now, &parks, vehicles, &mut walking, resources) {
                return;
            }
            h.decision.kind = HumanDecisionKind::Yield;
        }
        if !h.decision.kind.update(
            &mut h.router,
            infos,
            map,
            freight,
            cbuf,
            id,
            h.work.as_mut(),
        ) {
            return;
        }
        if !h.household.carried.is_empty() && h.location == Location::Building(h.home.house) {
            h.household.deposit(now, resources);
        }
        let carrying = !h.household.carried.is_empty();
        if carrying && now >= h.household.next_deposit {
            h.decision.kind = HumanDecisionKind::DepositAtHome;
            return;
        }

        let home_score = h.home.score(h.household.needs.rest);
        h.home.last_score = home_score;
        let at_work = h.attended_work_here(time, infos);
        let after_work = at_work
            && h.work
                .as_ref()
                .is_some_and(|work| !work.work_inter.is_active(&time.daytime));
        let food = h.food.score(&h.household);
        let shopping = if h.household.can_shop(now) {
            let staples = h.household.units(household_items()[0]);
            if after_work && staples <= 2 {
                0.9
            } else {
                food.max(BuyGoods::basket_score(&h.household))
            }
        } else {
            0.0
        };
        // An urgent pantry can use the last part of a shift. Stagger release
        // times so all workers at a site do not request the same shelves.
        let shopping_break = shopping > 0.0
            && food > 0.0
            && at_work
            && h.work.as_ref().is_some_and(|work| {
                matches!(work.kind, WorkKind::Worker)
                    && work.shift_ending(time, 10 + (common::hash_u64(id) % 20) as i32)
            });
        if shopping_break
            && start_basket(
                h,
                id,
                now,
                origin,
                &parks,
                vehicles,
                &mut walking,
                resources,
            )
        {
            return;
        }

        if let Some(work) = h.work.as_mut() {
            let attended = match (work.employer, infos.owner(work.workplace)) {
                (None, _) => h.household.attended_shift,
                (Some(_), Some(SoulID::GoodsCompany(employer))) => work
                    .attended_today(time, Some(employer))
                    .then_some((work.workplace, time.daytime.day)),
                _ => None,
            };
            let score = work.shift_score(time, &h.location, attended);
            work.last_score = score;
            if score > home_score {
                h.decision.kind = if h.location == Location::Building(work.workplace) {
                    if !matches!(
                        work.kind,
                        WorkKind::Driver {
                            deliver_order: Some(_),
                            ..
                        }
                    ) {
                        HumanDecisionKind::Yield
                    } else {
                        work.apply(&h.location, &h.router)
                    }
                } else {
                    work.apply(&h.location, &h.router)
                };
                if let Location::Building(building) = h.location {
                    h.attend_work_for(time, building, infos);
                }
                return;
            }
        }

        // Explicit legacy single-item orders can still finish through the old
        // pickup interface. The household scorer only places basket orders.
        if h.bought
            .0
            .get(&ItemID::new("retail-goods"))
            .is_some_and(|trades| !trades.is_empty())
        {
            h.decision.kind = h.goods.apply(
                cbuf,
                infos,
                time,
                id,
                &h.trans,
                &mut h.router,
                &mut h.bought,
            );
            return;
        }

        let leisure = leisure_score(&h.household, time, now);
        let can_walk = matches!(h.location, Location::Building(_) | Location::Outside);
        if can_walk
            && shopping > home_score.max(leisure)
            && start_basket(
                h,
                id,
                now,
                origin,
                &parks,
                vehicles,
                &mut walking,
                resources,
            )
        {
            return;
        }
        if can_walk && leisure > home_score {
            h.household.next_leisure =
                now + 3 * TICKS_PER_HOUR + common::hash_u64(id) % (30 * TICKS_PER_MINUTE);
            let park = parks.claim(
                &resources.read::<HouseholdClaims>(),
                &mut walking,
                origin,
                h.pedestrian.walking_speed,
                &h.location,
            );
            if let Some(park) = park {
                h.household.leisure_visit = Some(park);
                h.household.leisure_started = now;
                resources
                    .write::<HouseholdClaims>()
                    .add_park_claim(park, id, &h.household);
                h.start_walk(park, TripPurpose::Leisure, resources);
                h.decision.kind = HumanDecisionKind::VisitLeisure(park);
                return;
            }
        }
        h.decision.kind = if carrying {
            HumanDecisionKind::Yield
        } else {
            h.go_home()
        };
    });
}

#[allow(clippy::too_many_arguments)]
fn start_basket(
    h: &mut HumanEnt,
    shopper: HumanID,
    now: u64,
    origin: Vec3,
    parks: &ParkChoices,
    vehicles: &HopSlotMap<VehicleID, VehicleEnt>,
    walking: &mut WalkingReach,
    resources: &Resources,
) -> bool {
    let request = h.household.place_order(now);
    let travel_budget = BASKET_TRAVEL_LIMIT - 60 * TICKS_PER_REALTIME_SECOND;
    let walk_radius =
        h.pedestrian.walking_speed * travel_budget as f32 / TICKS_PER_REALTIME_SECOND as f32;
    let radius = if h.attended_work_here(&resources.read::<GameTime>(), parks.infos) {
        walk_radius.min(AFTER_WORK_RADIUS)
    } else {
        walk_radius
    };
    let mut component = None;
    let map = parks.map;
    let preferred = map
        .get(h.home.house)
        .filter(|home| {
            home.is_mixed_use()
                && !home.is_abandoned_growable()
                && home.development().is_some_and(|site| {
                    site.business == Some(crate::map::GrowableBusinessState::Open)
                })
                && parks.infos.residence_of(SoulID::Human(shopper)) == Some(home.id)
        })
        .and_then(|home| parks.infos.owner(home.id));
    let mut market = resources.write::<Market>();
    let reachable = |seller| {
        let store = map.get(parks.infos.building_owned_by(seller)?)?;
        if store.is_orphaned()
            || store.development().is_some_and(|site| {
                site.business != Some(crate::map::GrowableBusinessState::Open)
                    || store.is_abandoned_growable()
            })
        {
            return None;
        }
        if h.location != Location::Building(store.id) {
            let start = *component.get_or_insert_with(|| walking.component_at(map, origin));
            if !start.is_some_and(|start| walking.reaches_building(map, start, store))
                || !parks.walk_fits(origin, store, h.pedestrian.walking_speed, travel_budget)
            {
                return None;
            }
        }
        Some(store.id)
    };
    let basket = if preferred.is_some() {
        market.reserve_basket_preferred(
            shopper,
            origin.xy(),
            radius,
            request,
            now,
            preferred,
            h.location == Location::Building(h.home.house),
            reachable,
        )
    } else {
        market.reserve_basket(shopper, origin.xy(), radius, request, now, reachable)
    };
    drop(market);
    let (basket, missing) = match basket {
        Ok(selection) => selection,
        Err(reason) => {
            if reason == BasketFailure::NoStockInRange {
                h.household
                    .note_shelf_shortfall(now, [request.staples, request.goods]);
            }
            h.household.shop_failed(now, reason, resources);
            return false;
        }
    };
    let (id, store) = (basket.id, basket.building);
    if !h.household.reserve(basket, resources) {
        return false;
    }
    h.household.note_shelf_shortfall(now, missing);
    if h.location != Location::Building(store) {
        h.start_walk(store, TripPurpose::Shopping, resources);
    }
    h.decision.kind = HumanDecisionKind::PickupBasket(id);
    // A resident may already be inside the selected store.
    h.update_household_action(shopper, now, parks, vehicles, walking, resources);
    true
}

pub fn spawn_human(sim: &mut Simulation, house: BuildingID) -> Option<HumanID> {
    profiling::scope!("spawn_human");
    let map = sim.map();
    let housepos = map
        .get(house)
        .filter(|house| !house.is_orphaned() && !house.is_abandoned_growable())?
        .door_pos;
    drop(map);
    {
        let infos = sim.read::<BuildingInfos>();
        let residences = sim.read::<Residences>();
        let residence = residences.by_building.get(&house)?;
        if residences.is_abandoned(house)
            || infos.get(house)?.residents.len() >= residence.capacity as usize
        {
            return None;
        }
    }
    let _color = random_pedestrian_shirt_color(&mut sim.write::<RandProvider>());

    let p = Pedestrian::new(&mut sim.write::<RandProvider>());

    let time = sim.read::<GameTime>().instant();

    let personal_info = Box::new(PersonalInfo::new(&mut sim.write::<RandProvider>()));

    let id = sim.world.insert(HumanEnt {
        trans: Transform::new(housepos),
        location: Location::Building(house),
        pedestrian: p,
        it: Itinerary::NONE,
        speed: Speed::default(),
        decision: HumanDecision::default(),
        home: Home::new(house),
        food: BuyFood::new(time),
        goods: BuyGoods::default(),
        bought: Bought::default(),
        router: Router::new(None),
        collider: None,
        work: None,
        personal_info,
        household: Box::default(),
    });

    let now = sim
        .read::<crate::economy::Budget>()
        .elapsed_ticks(sim.read::<GameTime>().tick);
    sim.world
        .edit_human(id, |human| human.household.initialize(id, now));

    let key = id.data().as_ffi();
    if common::rand::randu(key as u32 ^ (key >> 32) as u32) < 0.7 {
        let car = spawn_parked_vehicle(sim, VehicleKind::Car, housepos);
        sim.world
            .edit_human(id, |human| human.router = Router::new(car));
    }

    let soul = SoulID::Human(id);
    let mut m = sim.write::<Market>();
    m.buy(soul, housepos.xy(), ItemID::new("job-opening"), 1);

    let mut infos = sim.write::<BuildingInfos>();
    infos.get_in(house, soul);
    infos.add_resident(house, soul);

    Some(id)
}

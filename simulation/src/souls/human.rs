use crate::economy::cargo::{complete_delivery, settle_delivery, LoadId};
use crate::economy::{Bought, DailyEconomyMetrics, Market, TripPurpose};
use crate::map::BuildingID;
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{
    BuildingInfos, Destination, ElectricityFlow, Itinerary, ParkingManagement, Router, WalkingReach,
};
use crate::souls::desire::{
    leisure_score, BuyFood, BuyGoods, Home, ParkChoices, Work, WorkKind, AFTER_WORK_RADIUS,
    PARK_DWELL, PARK_TRAVEL_LIMIT,
};
use crate::souls::household::{cancel_visit, BasketId, HouseholdStock};
use crate::souls::residence::Residences;
use crate::transportation::Speed;
use crate::transportation::{
    random_pedestrian_shirt_color, spawn_parked_vehicle, Location, Pedestrian, VehicleKind,
};
use crate::utils::rand_provider::RandProvider;
use crate::utils::resources::Resources;
use crate::world::{FreightStationEnt, HumanEnt, HumanID, VehicleID};
use crate::World;
use crate::{BuildingKind, Map, ParCommandBuffer, Simulation, SoulID};
use egui_inspect::Inspect;
use geom::{Transform, Vec3};
use lazy_static::lazy_static;
use prototypes::{GameTime, ItemID, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use slotmapd::Key;

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
    pub(crate) fn references_building(&self, building: BuildingID) -> bool {
        match self {
            Self::GoTo(Destination::Building(id))
            | Self::DeliverAtBuilding(id)
            | Self::DeliverLoad {
                destination: id, ..
            }
            | Self::VisitLeisure(id) => *id == building,
            Self::MultiStack(steps) => steps.iter().any(|step| step.references_building(building)),
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

    pub fn update(
        &mut self,
        router: &mut Router,
        binfos: &BuildingInfos,
        map: &Map,
        cbuf_freight: &ParCommandBuffer<FreightStationEnt>,
        cbuf: &ParCommandBuffer<HumanEnt>,
        driver: HumanID,
        location: &Location,
        home: BuildingID,
        mut work: Option<&mut Work>,
    ) -> bool {
        match *self {
            HumanDecisionKind::GoTo(dest) => {
                let purpose = trip_purpose(dest, home, work.as_deref(), router);
                router.go_to_for(dest, purpose)
            }
            HumanDecisionKind::MultiStack(ref mut decisions) => {
                if let Some(d) = decisions.last_mut() {
                    if d.update(
                        router,
                        binfos,
                        map,
                        cbuf_freight,
                        cbuf,
                        driver,
                        location,
                        home,
                        work,
                    ) {
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
            HumanDecisionKind::DeliverAtBuilding(bid)
            | HumanDecisionKind::DeliverLoad {
                destination: bid, ..
            } => {
                if let (Self::DeliverLoad { load, .. }, Some(WorkKind::Driver { truck, .. })) =
                    (&*self, work.as_deref().map(|work| work.kind))
                {
                    if complete_delivery(*load, truck, bid) {
                        return true;
                    }
                }
                if settle_delivery(cbuf, driver, location, bid, work.as_deref_mut()) {
                    return true;
                }
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
                        if let Some(f) = e.world.freight_stations.get_mut(fid) {
                            f.f.waiting_cargo += 1;
                        }
                    });
                }
                true
            }
            HumanDecisionKind::Yield
            | HumanDecisionKind::PickupBasket(_)
            | HumanDecisionKind::DepositAtHome
            | HumanDecisionKind::VisitLeisure(_) => true,
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

fn trip_purpose(
    dest: Destination,
    home: BuildingID,
    work: Option<&Work>,
    router: &Router,
) -> TripPurpose {
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
        if building == work.workplace {
            return TripPurpose::Work;
        }
    }
    if building == home {
        TripPurpose::Home
    } else {
        TripPurpose::Other
    }
}

impl HumanEnt {
    pub(crate) fn attend_work(&mut self, time: &GameTime, building: BuildingID) {
        if self.work.as_ref().is_some_and(|work| {
            work.workplace == building && work.work_inter.dist_start(&time.daytime) == 0
        }) {
            let day = self.household.attended_day.unwrap_or(time.daytime.day);
            self.household.attended_day = Some(day.max(time.daytime.day));
        }
    }

    fn after_work(&self) -> bool {
        self.work
            .as_ref()
            .is_some_and(|work| self.location == Location::Building(work.workplace))
    }

    fn go_home(&mut self) -> HumanDecisionKind {
        if self.location == Location::Building(self.home.house) {
            HumanDecisionKind::Yield
        } else {
            self.home.apply()
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
        self.router
            .cancel_route(&mut resources.write::<ParkingManagement>());
        self.it = Itinerary::NONE;
    }

    fn update_household_action(
        &mut self,
        now: u64,
        map: &Map,
        infos: &BuildingInfos,
        walking: &mut WalkingReach,
        resources: &Resources,
    ) -> bool {
        match self.decision.kind {
            HumanDecisionKind::PickupBasket(id) => {
                let Some(basket) = self.household.reservation.as_ref() else {
                    return true;
                };
                let seller_present = infos.owner(basket.building) == Some(basket.seller);
                let store = map.get(basket.building);
                if !seller_present
                    || store.is_none_or(|store| store.is_orphaned())
                    || now >= basket.expires
                    || self.router.last_error.is_some()
                    || (self.location != Location::Building(basket.building)
                        && self.router.target_dest != Some(Destination::Building(basket.building)))
                {
                    self.household.cancel(resources, seller_present);
                    self.router.walk_only_pickup = false;
                    self.stop_route(resources);
                    true
                } else if self.location == Location::Building(basket.building) {
                    self.household.collect(id, now, resources);
                    self.router.walk_only_pickup = false;
                    self.stop_route(resources);
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
                if self.router.walk_only_pickup
                    && (self.router.last_error.is_some() || self.router.target_dest.is_none())
                {
                    self.router.walk_only_pickup = false;
                    self.stop_route(resources);
                    self.router.go_to_for(dest, TripPurpose::Home);
                } else if self.router.target_dest != Some(dest) {
                    let origin = match self.location {
                        Location::Building(id) => {
                            map.get(id).map_or(self.trans.pos, |b| b.door_pos)
                        }
                        _ => self.trans.pos,
                    };
                    let walk_home = map.get(home).is_some_and(|building| {
                        let start = walking.component_at(map, origin);
                        start.is_some_and(|start| walking.reaches_building(map, start, building))
                    });
                    if walk_home {
                        self.start_walk(home, TripPurpose::Home, resources);
                    } else {
                        self.router.walk_only_pickup = false;
                        self.router.go_to_for(dest, TripPurpose::Home);
                    }
                }
                false
            }
            HumanDecisionKind::VisitLeisure(park) => {
                if map.get(park).is_none_or(|park| park.is_orphaned())
                    || self.router.last_error.is_some()
                    || (self.location != Location::Building(park)
                        && self.router.target_dest != Some(Destination::Building(park)))
                    || (self.household.leisure_until.is_none()
                        && now - self.household.leisure_started >= PARK_TRAVEL_LIMIT)
                {
                    cancel_visit(self);
                    self.stop_route(resources);
                    return true;
                }
                if self.location != Location::Building(park) {
                    return false;
                }
                let until = *self.household.leisure_until.get_or_insert(now + PARK_DWELL);
                if now < until {
                    return false;
                }
                self.household.needs.leisure = 1.0;
                self.household.next_leisure = now + 3 * 24 * TICKS_PER_HOUR;
                cancel_visit(self);
                self.stop_route(resources);
                true
            }
            _ => true,
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
    let mut walking = WalkingReach::default();
    let mut park_requests = Vec::new();

    for (id, h) in world.humans.iter_mut() {
        BuyGoods::update_pickup(h, time, map, vehicles, resources);
        let pickup_interrupted = h
            .household
            .reservation
            .as_ref()
            .is_some_and(|basket| now >= basket.expires || h.router.last_error.is_some());
        if h.decision.wait_one_tick() && !pickup_interrupted {
            continue;
        }
        h.household.refresh_needs(
            now,
            h.location == Location::Building(h.home.house),
            resources,
        );
        let origin = match h.location {
            Location::Building(id) => map.get(id).map_or(h.trans.pos, |b| b.door_pos),
            _ => h.trans.pos,
        };
        h.decision.wait = (30.0 + common::rand::rand2(origin.x, origin.y) * 50.0) as u8;

        if matches!(
            h.decision.kind,
            HumanDecisionKind::PickupBasket(_)
                | HumanDecisionKind::DepositAtHome
                | HumanDecisionKind::VisitLeisure(_)
        ) {
            if !h.update_household_action(now, map, infos, &mut walking, resources) {
                continue;
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
            &h.location,
            h.home.house,
            h.work.as_mut(),
        ) {
            continue;
        }
        if !h.household.carried.is_empty() {
            h.decision.kind = HumanDecisionKind::DepositAtHome;
            continue;
        }

        let home_score = h.home.score(h.household.needs.rest);
        h.home.last_score = home_score;
        if let Some(work) = h.work.as_mut() {
            let score = work.shift_score(time, &h.location, h.household.attended_day);
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
                    h.attend_work(time, building);
                }
                continue;
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
            continue;
        }

        let food = h.food.score(&h.household);
        let shopping = if h.household.can_shop(now) {
            food.max(BuyGoods::basket_score(&h.household))
        } else {
            0.0
        };
        let leisure = leisure_score(&h.household, time, now);
        if shopping > home_score.max(leisure)
            && start_basket(h, id, now, origin, map, infos, &mut walking, resources)
        {
            continue;
        }
        if leisure > home_score {
            park_requests.push((id, origin));
        } else {
            h.decision.kind = h.go_home();
        }
    }

    if !park_requests.is_empty() {
        let mut parks = ParkChoices::new(world, map, infos, &resources.read::<ElectricityFlow>());
        for (id, origin) in park_requests {
            let h = &mut world.humans[id];
            h.household.next_leisure = now + TICKS_PER_HOUR;
            if let Some(park) = parks.claim(map, &mut walking, origin, &h.location, h.after_work())
            {
                h.household.leisure_visit = Some(park);
                h.household.leisure_started = now;
                h.start_walk(park, TripPurpose::Leisure, resources);
                h.decision.kind = HumanDecisionKind::VisitLeisure(park);
            } else {
                h.decision.kind = h.go_home();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_basket(
    h: &mut HumanEnt,
    shopper: HumanID,
    now: u64,
    origin: Vec3,
    map: &Map,
    infos: &BuildingInfos,
    walking: &mut WalkingReach,
    resources: &Resources,
) -> bool {
    let request = h
        .household
        .place_order(now, &mut resources.write::<DailyEconomyMetrics>());
    let radius = if h.after_work() {
        AFTER_WORK_RADIUS
    } else {
        1_500.0
    };
    let mut component = None;
    let basket =
        resources
            .write::<Market>()
            .reserve_basket(shopper, origin.xy(), request, now, |seller| {
                let store = map.get(infos.building_owned_by(seller)?)?;
                if store.is_orphaned() || store.door_pos.distance2(origin) > radius * radius {
                    return None;
                }
                if h.location != Location::Building(store.id) {
                    let start = *component.get_or_insert_with(|| walking.component_at(map, origin));
                    if !start.is_some_and(|start| walking.reaches_building(map, start, store)) {
                        return None;
                    }
                }
                Some((store.id, store.door_pos.xy()))
            });
    let Some(basket) = basket else {
        return false;
    };
    h.start_walk(basket.building, TripPurpose::Shopping, resources);
    h.decision.kind = HumanDecisionKind::PickupBasket(basket.id);
    h.household.reserve(basket);
    // A resident may already be inside the selected store.
    h.update_household_action(now, map, infos, walking, resources);
    true
}

pub fn spawn_human(sim: &mut Simulation, house: BuildingID) -> Option<HumanID> {
    profiling::scope!("spawn_human");
    let map = sim.map();
    let housepos = map
        .get(house)
        .filter(|house| !house.is_orphaned())?
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
        household: HouseholdStock::default(),
    });

    let now = sim
        .read::<crate::economy::Budget>()
        .elapsed_ticks(sim.read::<GameTime>().tick);
    sim.world.humans[id].household.initialize(id, now);

    let key = id.data().as_ffi();
    if common::rand::randu(key as u32 ^ (key >> 32) as u32) < 0.7 {
        let car = spawn_parked_vehicle(sim, VehicleKind::Car, housepos);
        sim.world.humans[id].router = Router::new(car);
    }

    let soul = SoulID::Human(id);
    let mut m = sim.write::<Market>();
    m.buy(soul, housepos.xy(), ItemID::new("job-opening"), 1);

    let mut infos = sim.write::<BuildingInfos>();
    infos.get_in(house, soul);
    infos.add_resident(house, soul);

    Some(id)
}

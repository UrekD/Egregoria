use std::collections::{BTreeMap, BTreeSet, VecDeque};

use common::saveload::Encoder;
use geom::{Transform, Vec3};
use prototypes::{GameTime, Money, RecTimeInterval, ServiceID, Tick, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};

use super::events::{elapsed_tick, fire_incident, ServiceIncidents};
use super::service_access::{
    check_apron, current_anchor, drive_route, is_fire_station, AccessBudget, AccessError,
    DirectedLaneAnchor, FireApronPreview, APRON_WORK,
};
use crate::map::{BuildingID, Map, RoadID, Traversable, TraverseKind};
use crate::map_dynamic::{
    active_business, BuildingInfos, ElectricityFlow, Itinerary, ParkingManagement,
};
use crate::transportation::{
    put_pedestrian_in_transport_grid, put_vehicle_in_transport_grid, Location, TransportGrid,
    VehicleKind, VehicleState,
};
use crate::world_command::Rejection;
use crate::{HumanID, Simulation, VehicleID};

const STATION_LIMIT: usize = 128;
const SEARCH_WORK: usize = 8192;
const WATCHDOG: u64 = 15 * TICKS_PER_MINUTE;
const HOLD_DEADLINE: u64 = 30 * TICKS_PER_MINUTE;
const RETRY_TICKS: u64 = TICKS_PER_MINUTE;
const HISTORY_LIMIT: usize = 1024;
pub(crate) const SUPPRESSION_MINUTES: u64 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FireApronQuote {
    pub signature: u64,
    pub curb_spaces: u16,
    pub price: Money,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceTripPhase {
    Boarding,
    Merging,
    Outbound,
    Parking,
    WalkingToSite,
    Working,
    WalkingToTruck,
    LeavingScene,
    Returning,
    Docking,
    LeavingTruck,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FireResponseBlocker {
    Staffing,
    Power,
    SceneAccess(AccessError),
    Route(AccessError),
    CrewLost,
    Held,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FireResponseEventKind {
    Assigned,
    Boarded,
    Arrived,
    Suppressed,
    Returned,
    Recovered,
    Held,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FireResponseEvent {
    pub station: BuildingID,
    pub site: BuildingID,
    pub incident: u64,
    pub position: Vec3,
    pub tick: Tick,
    pub kind: FireResponseEventKind,
}

#[derive(Clone, Copy, Debug)]
pub struct FireStationView {
    pub truck: VehicleID,
    pub phase: Option<ServiceTripPhase>,
    pub incident: Option<BuildingID>,
    pub blocker: Option<FireResponseBlocker>,
    pub crew: [Option<HumanID>; 8],
    pub overtime_ticks: u64,
    pub returned: u64,
    pub recovered: u64,
}

#[derive(Serialize, Deserialize)]
struct Station {
    apron: FireApronPreview,
    truck: VehicleID,
    roster: [Option<HumanID>; 8],
    trip: Option<Trip>,
    overtime_ticks: u64,
    returned: u64,
    recovered: u64,
}

#[derive(Serialize, Deserialize)]
struct Trip {
    site: BuildingID,
    incident: u64,
    position: Vec3,
    scene: FireApronPreview,
    scene_reserved: bool,
    crew: [Option<HumanID>; 2],
    phase: ServiceTripPhase,
    phase_since: Tick,
    last_progress: Tick,
    last_positions: [Vec3; 3],
    #[serde(deserialize_with = "decode_route")]
    outbound: Option<Vec<Traversable>>,
    #[serde(deserialize_with = "decode_route")]
    inbound: Option<Vec<Traversable>>,
    #[serde(skip)]
    graph: (u64, u64),
    failed: bool,
    suppressed: bool,
    hold_reported: bool,
    retry_at: Tick,
}

#[derive(Default, Serialize, Deserialize)]
pub struct FireFleet {
    #[serde(deserialize_with = "decode_stations")]
    stations: BTreeMap<BuildingID, Station>,
    #[serde(skip)]
    deployed: BTreeSet<HumanID>,
    #[serde(skip)]
    outbound: BTreeMap<VehicleID, (BuildingID, u64, [HumanID; 2])>,
    #[serde(skip)]
    active_stations: BTreeSet<BuildingID>,
    #[serde(skip)]
    trucks: BTreeSet<VehicleID>,
    #[serde(skip)]
    assignments: BTreeMap<u64, BuildingID>,
    #[serde(deserialize_with = "decode_history")]
    history: VecDeque<FireResponseEvent>,
    #[serde(deserialize_with = "decode_alarms")]
    alarms: BTreeMap<(Tick, BuildingID, u64), Alarm>,
    dispatch_at: Tick,
    roster_at: Tick,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Alarm {
    due: Tick,
    blocker: Option<FireResponseBlocker>,
}

fn decode_stations<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<BuildingID, Station>, D::Error> {
    decode_map(d, STATION_LIMIT)
}

fn decode_alarms<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<(Tick, BuildingID, u64), Alarm>, D::Error> {
    decode_map(d, 4096)
}

fn decode_map<'de, D, K, V>(d: D, limit: usize) -> Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Ord + Deserialize<'de>,
    V: Deserialize<'de>,
{
    struct Visitor<K, V>(usize, std::marker::PhantomData<(K, V)>);
    impl<'de, K, V> serde::de::Visitor<'de> for Visitor<K, V>
    where
        K: Ord + Deserialize<'de>,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "at most {} unique fire fleet records", self.0)
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some(key) = map.next_key()? {
                if out.len() == self.0 || out.contains_key(&key) {
                    return Err(serde::de::Error::custom("invalid fire fleet map"));
                }
                out.insert(key, map.next_value()?);
            }
            Ok(out)
        }
    }
    d.deserialize_map(Visitor(limit, std::marker::PhantomData))
}

fn decode_route<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<Traversable>>, D::Error> {
    #[derive(Deserialize)]
    struct Route(
        #[serde(
            deserialize_with = "crate::world_command::bounded_command_vec::<_, _, SEARCH_WORK>"
        )]
        Vec<Traversable>,
    );
    Ok(Option::<Route>::deserialize(d)?.map(|route| route.0))
}

fn decode_history<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<VecDeque<FireResponseEvent>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = VecDeque<FireResponseEvent>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "at most {HISTORY_LIMIT} fire response events")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = VecDeque::new();
            while let Some(event) = seq.next_element()? {
                if out.len() == HISTORY_LIMIT {
                    return Err(serde::de::Error::custom("too many fire response events"));
                }
                out.push_back(event);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(Visitor)
}

impl FireFleet {
    pub fn events(&self) -> impl Iterator<Item = &FireResponseEvent> {
        self.history.iter()
    }

    pub(crate) fn deployed(&self, id: HumanID) -> bool {
        self.deployed.contains(&id)
    }

    pub(crate) fn owns_truck(&self, truck: VehicleID) -> bool {
        self.trucks.contains(&truck)
    }

    pub(crate) fn is_live_outbound(
        &self,
        world: &crate::World,
        incidents: &ServiceIncidents,
        vehicle: VehicleID,
    ) -> bool {
        self.outbound
            .get(&vehicle)
            .is_some_and(|&(site, generation, crew)| {
                incidents
                    .incident(site)
                    .is_some_and(|fire| fire.id == generation)
                    && crew.iter().all(|&id| {
                        world
                            .humans
                            .get(id)
                            .is_some_and(|human| human.location == Location::Vehicle(vehicle))
                    })
            })
    }

    pub(crate) fn suppression_started(&self, site: BuildingID, incident: u64) -> Option<Tick> {
        let station = self.stations.get(self.assignments.get(&incident)?)?;
        let trip = station.trip.as_ref()?;
        (trip.site == site
            && trip.incident == incident
            && trip.phase == ServiceTripPhase::Working
            && !trip.failed)
            .then_some(trip.phase_since)
    }

    fn record(&mut self, station: BuildingID, trip: &Trip, now: Tick, kind: FireResponseEventKind) {
        if self.history.len() == HISTORY_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(FireResponseEvent {
            station,
            site: trip.site,
            incident: trip.incident,
            position: trip.position,
            tick: now,
            kind,
        });
    }
}

pub fn service_station(sim: &Simulation, site: BuildingID) -> Option<FireStationView> {
    let fleet = sim.resources.read::<FireFleet>();
    let station = fleet.stations.get(&site)?;
    let trip = station.trip.as_ref();
    Some(FireStationView {
        truck: station.truck,
        phase: trip.map(|trip| trip.phase),
        incident: trip.map(|trip| trip.site),
        blocker: match trip {
            Some(trip) if trip.hold_reported => Some(FireResponseBlocker::Held),
            Some(trip) if trip.failed => Some(FireResponseBlocker::CrewLost),
            Some(_) => None,
            None => available_crew(sim, site, station).err(),
        },
        crew: station.roster,
        overtime_ticks: station.overtime_ticks,
        returned: station.returned,
        recovered: station.recovered,
    })
}

pub fn fire_response_blocker(sim: &Simulation, site: BuildingID) -> Option<FireResponseBlocker> {
    let incident = fire_incident(sim, site)?;
    sim.resources
        .read::<FireFleet>()
        .alarms
        .get(&(incident.alarm_tick, site, incident.id))?
        .blocker
}

pub(crate) fn commissioned_apron(sim: &Simulation, site: BuildingID) -> Option<FireApronPreview> {
    Some(
        sim.resources
            .read::<FireFleet>()
            .stations
            .get(&site)?
            .apron
            .clone(),
    )
}

fn apron_quote(apron: &FireApronPreview) -> FireApronQuote {
    let signature =
        common::hash_iter(common::saveload::Bincode::encode(apron).expect("serializable apron"));
    let curb_spaces = apron.parking_displacements.len() as u16;
    FireApronQuote {
        signature,
        curb_spaces,
        price: Money::new_bucks(20 * i64::from(curb_spaces)),
    }
}

pub fn fire_apron_quote(sim: &Simulation, site: BuildingID) -> Result<FireApronQuote, AccessError> {
    if sim
        .resources
        .read::<FireFleet>()
        .stations
        .contains_key(&site)
    {
        return Err(AccessError::AlreadyCommissioned);
    }
    super::service_access::preview_fire_apron(sim, site).map(|apron| apron_quote(&apron))
}

pub(crate) fn prepare_new_apron(
    sim: &Simulation,
    road: Option<RoadID>,
    door: Vec3,
    footprint: geom::OBB,
    remaining: &mut usize,
) -> Result<(FireApronPreview, Money), Rejection> {
    let apron = prepare_apron(sim, road, door, Some(footprint), remaining)?;
    let quote = apron_quote(&apron);
    Ok((apron, quote.price))
}

pub(crate) fn prepare_retrofit(
    sim: &Simulation,
    site: BuildingID,
    quote: FireApronQuote,
    remaining: &mut usize,
) -> Result<FireApronPreview, Rejection> {
    if sim
        .resources
        .read::<FireFleet>()
        .stations
        .contains_key(&site)
    {
        return Err(Rejection::FireAccess(AccessError::AlreadyCommissioned));
    }
    let map = sim.map();
    let building = map
        .get(site)
        .filter(|building| is_fire_station(building.kind))
        .ok_or(Rejection::FireAccess(AccessError::MissingStation))?;
    let apron = prepare_apron(
        sim,
        building.connected_road,
        building.door_pos,
        None,
        remaining,
    )?;
    if apron_quote(&apron) != quote {
        return Err(Rejection::StaleFireApronQuote);
    }
    Ok(apron)
}

fn prepare_apron(
    sim: &Simulation,
    road: Option<RoadID>,
    door: Vec3,
    footprint: Option<geom::OBB>,
    remaining: &mut usize,
) -> Result<FireApronPreview, Rejection> {
    if sim.resources.read::<FireFleet>().stations.len() >= STATION_LIMIT {
        return Err(Rejection::FireAccess(AccessError::WorkLimit));
    }
    let allowance = (*remaining).min(APRON_WORK);
    let mut work = AccessBudget(allowance);
    let apron = check_apron(sim, road, door, footprint, &mut work);
    *remaining -= allowance - work.0;
    apron.map_err(Rejection::FireAccess)
}

fn guard_frontage(sim: &Simulation, site: BuildingID, apron: &FireApronPreview) {
    sim.map_mut()
        .service_frontages
        .entry(apron.road)
        .or_default()
        .push((site, apron.envelope));
}

fn release_frontage(sim: &Simulation, site: BuildingID, apron: &FireApronPreview) {
    let road = apron.road;
    let mut map = sim.map_mut();
    let frontages = map
        .service_frontages
        .get_mut(&road)
        .expect("occupied service frontage");
    frontages.retain(|(owner, shape)| *owner != site || shape.corners != apron.envelope.corners);
    if frontages.is_empty() {
        map.service_frontages.remove(&road);
    }
}

pub(crate) fn commission(sim: &mut Simulation, site: BuildingID, apron: FireApronPreview) {
    assert!(sim
        .resources
        .write::<ParkingManagement>()
        .reserve_service_curb(&apron.parking_displacements));
    guard_frontage(sim, site, &apron);
    let truck = crate::transportation::spawn_fire_truck(sim, apron.bay);
    let mut fleet = sim.resources.write::<FireFleet>();
    assert!(fleet
        .stations
        .insert(
            site,
            Station {
                apron,
                truck,
                roster: [None; 8],
                trip: None,
                overtime_ticks: 0,
                returned: 0,
                recovered: 0,
            }
        )
        .is_none());
    fleet.trucks.insert(truck);
    fleet.roster_at = Tick(0);
}

pub(crate) fn clearance_blocked(sim: &Simulation, site: BuildingID) -> bool {
    sim.resources
        .read::<FireFleet>()
        .stations
        .iter()
        .any(|(&station_site, station)| {
            station.trip.as_ref().is_some_and(|trip| {
                station_site == site || (trip.site == site && trip.scene_reserved)
            })
        })
}

pub(crate) fn release_station(sim: &mut Simulation, site: BuildingID) {
    let station = sim.resources.write::<FireFleet>().stations.remove(&site);
    if let Some(station) = station {
        assert!(station.trip.is_none(), "occupied station clearance");
        sim.resources
            .write::<FireFleet>()
            .trucks
            .remove(&station.truck);
        release_frontage(sim, site, &station.apron);
        sim.resources
            .write::<ParkingManagement>()
            .release_service_curb(&station.apron.parking_displacements);
        sim.world.vehicles.remove(station.truck);
    }
}

fn shift(seat: usize, daytime: &prototypes::DayTime) -> RecTimeInterval {
    match seat {
        0..=5 => {
            let start = (seat / 2) as i32 * 8;
            RecTimeInterval::new((start, 0), ((start + 8) % 24, 0))
        }
        // The relief seats cover staggered windows at the next turnover.
        6..=7 => {
            let seconds = daytime.daysec();
            let start = (seconds + 45 * 60) / (8 * 3600) * (8 * 3600);
            let start_before = if seat == 6 { 45 } else { 30 };
            let end_after = if seat == 6 { 30 } else { 45 };
            RecTimeInterval::new_daysec(
                (start - start_before * 60).rem_euclid(24 * 3600),
                (start + end_after * 60).rem_euclid(24 * 3600),
            )
        }
        _ => unreachable!("stock roster seats"),
    }
}

fn update_rosters(sim: &mut Simulation, now: Tick) {
    let daytime = sim.resources.read::<GameTime>().daytime;
    let mut fleet = sim.resources.write::<FireFleet>();
    if now < fleet.roster_at {
        return;
    }
    fleet.roster_at = Tick(now.0 + TICKS_PER_MINUTE);
    let map = sim.resources.read::<Map>();
    let infos = sim.resources.read::<BuildingInfos>();
    for (&site, station) in &mut fleet.stations {
        let Some(company) = map
            .get(site)
            .and_then(|building| active_business(building, &infos, &sim.world))
            .and_then(|business| sim.world.companies.get(business.company))
        else {
            continue;
        };
        let mut hired: BTreeSet<_> = company
            .workers
            .0
            .iter()
            .copied()
            .filter(|id| {
                sim.world.humans.get(*id).is_some_and(|human| {
                    human
                        .work
                        .as_ref()
                        .is_some_and(|work| work.workplace == site)
                })
            })
            .collect();
        for seat in &mut station.roster {
            if seat.is_some_and(|id| !hired.remove(&id)) {
                *seat = None;
            }
        }
        for (index, seat) in station.roster.iter_mut().enumerate() {
            if seat.is_none() {
                *seat = hired.pop_first();
            }
            if let Some(id) = *seat {
                sim.world.edit_human(id, |human| {
                    if let Some(work) = &mut human.work {
                        work.work_inter = shift(index, &daytime);
                    }
                });
            }
        }
    }
}

fn available_crew(
    sim: &Simulation,
    station: BuildingID,
    record: &Station,
) -> Result<[HumanID; 2], FireResponseBlocker> {
    let map = sim.map();
    if map
        .get(station)
        .is_none_or(|building| !is_fire_station(building.kind))
    {
        return Err(FireResponseBlocker::Staffing);
    }
    if map
        .electricity
        .net_id(station)
        .is_none_or(|net| sim.resources.read::<ElectricityFlow>().blackout(net))
    {
        return Err(FireResponseBlocker::Power);
    }
    let daytime = sim.resources.read::<GameTime>().daytime;
    let mut staffed = record.roster.iter().enumerate().filter_map(|(seat, id)| {
        let id = (*id)?;
        let human = sim.get(id)?;
        (human.location == Location::Building(station)
            && human
                .work
                .as_ref()
                .is_some_and(|work| work.workplace == station)
            && shift(seat, &daytime).is_active(&daytime))
        .then_some(id)
    });
    let mut next = || staffed.next().ok_or(FireResponseBlocker::Staffing);
    Ok([next()?, next()?])
}

fn access_revision(map: &Map) -> (u64, u64) {
    (
        map.walking_topology_generation,
        map.driving_policy_generation,
    )
}

pub(crate) fn dispatch_fire(sim: &mut Simulation) {
    if sim.resources.read::<FireFleet>().stations.is_empty() {
        return;
    }
    let now = elapsed_tick(sim);
    let now_due = sim.resources.read::<FireFleet>().dispatch_at <= now;
    if !now_due {
        return;
    }
    update_rosters(sim, now);
    let next = {
        let incidents = sim.resources.read::<ServiceIncidents>();
        let mut fleet = sim.resources.write::<FireFleet>();
        fleet.alarms.retain(|&(_, site, id), _| {
            incidents
                .incident(site)
                .is_some_and(|active| active.id == id)
        });
        for fire in incidents.active() {
            if !fleet.assignments.contains_key(&fire.id) {
                fleet
                    .alarms
                    .entry((fire.alarm_tick, fire.site, fire.id))
                    .or_insert(Alarm {
                        due: now,
                        blocker: None,
                    });
            } else {
                fleet.alarms.remove(&(fire.alarm_tick, fire.site, fire.id));
            }
        }
        let next = fleet
            .alarms
            .iter()
            .find(|(_, alarm)| alarm.due <= now)
            .map(|(&key, _)| key);
        fleet.dispatch_at = if next.is_some() {
            Tick(now.0 + 1)
        } else {
            Tick(now.0 + 10)
        };
        next
    };
    let Some(key @ (_, site, incident)) = next else {
        return;
    };
    let result = prepare_trip(sim, site);
    let (station_id, crew, scene, outbound, inbound) = match result {
        Ok(result) => result,
        Err(reason) => {
            sim.resources.write::<FireFleet>().alarms.insert(
                key,
                Alarm {
                    due: Tick(now.0 + RETRY_TICKS),
                    blocker: Some(reason),
                },
            );
            return;
        }
    };
    if !sim
        .resources
        .write::<ParkingManagement>()
        .reserve_service_curb(&scene.parking_displacements)
    {
        return;
    }
    guard_frontage(sim, site, &scene);
    let graph = access_revision(&sim.map());
    let position = fire_incident(sim, site)
        .expect("active dispatch incident")
        .position;
    let station_apron = commissioned_apron(sim, station_id).expect("selected station");
    for (seat, &id) in crew.iter().enumerate() {
        walk_to_pocket(sim, id, &station_apron, seat);
    }
    let trip = Trip {
        site,
        incident,
        position,
        scene,
        scene_reserved: true,
        crew: crew.map(Some),
        phase: ServiceTripPhase::Boarding,
        phase_since: now,
        last_progress: now,
        last_positions: [Vec3::ZERO; 3],
        outbound: Some(outbound),
        inbound: Some(inbound),
        graph,
        failed: false,
        suppressed: false,
        hold_reported: false,
        retry_at: now,
    };
    let mut fleet = sim.resources.write::<FireFleet>();
    fleet.record(station_id, &trip, now, FireResponseEventKind::Assigned);
    fleet.deployed.extend(crew);
    fleet.active_stations.insert(station_id);
    fleet.assignments.insert(incident, station_id);
    fleet.stations.get_mut(&station_id).unwrap().trip = Some(trip);
    fleet.alarms.remove(&key);
}

type Assignment = (
    BuildingID,
    [HumanID; 2],
    FireApronPreview,
    Vec<Traversable>,
    Vec<Traversable>,
);

fn prepare_trip(sim: &Simulation, site: BuildingID) -> Result<Assignment, FireResponseBlocker> {
    let map = sim.map();
    let target = map.get(site).ok_or(FireResponseBlocker::SceneAccess(
        AccessError::MissingFrontage,
    ))?;
    let fleet = sim.resources.read::<FireFleet>();
    let mut options = Vec::new();
    let mut blocker = FireResponseBlocker::Staffing;
    for (&station_id, station) in &fleet.stations {
        if station.trip.is_some() || sim.get(station.truck).is_none() {
            continue;
        }
        let Some(spec) = map
            .get(station_id)
            .and_then(|building| building.kind.as_goods_company())
            .and_then(|id| prototypes::try_prototype(ServiceID::from(id)))
            .and_then(|proto| proto.fire)
        else {
            continue;
        };
        if !station
            .apron
            .door
            .is_close(target.door_pos, f32::from(spec.max_response_m))
        {
            continue;
        }
        match available_crew(sim, station_id, station) {
            Ok(crew) => options.push((station_id, crew, station)),
            Err(FireResponseBlocker::Power) => blocker = FireResponseBlocker::Power,
            Err(_) => {}
        }
    }
    if options.is_empty() {
        return Err(blocker);
    }
    let mut work = AccessBudget(SEARCH_WORK);
    let scene = check_apron(sim, target.connected_road, target.door_pos, None, &mut work)
        .map_err(FireResponseBlocker::SceneAccess)?;
    let mut best: Option<(
        (u64, BuildingID),
        [HumanID; 2],
        Vec<Traversable>,
        Vec<Traversable>,
    )> = None;
    for (station_id, crew, station) in options {
        if crew.iter().any(|id| fleet.deployed.contains(id)) {
            continue;
        }
        let (ms, out) = match drive_route(&map, station.apron.departure, scene.arrival, &mut work) {
            Ok(result) => result,
            Err(AccessError::NoReturnLoop) => continue,
            Err(reason) => return Err(FireResponseBlocker::Route(reason)),
        };
        let rank = (ms, station_id);
        if best.as_ref().is_some_and(|(current, ..)| rank >= *current) {
            continue;
        }
        let (_, back) = match drive_route(&map, scene.departure, station.apron.arrival, &mut work) {
            Ok(result) => result,
            Err(AccessError::NoReturnLoop) => continue,
            Err(reason) => return Err(FireResponseBlocker::Route(reason)),
        };
        best = Some((rank, crew, out, back));
    }
    let ((_, station), crew, outbound, inbound) =
        best.ok_or(FireResponseBlocker::Route(AccessError::NoReturnLoop))?;
    Ok((station, crew, scene, outbound, inbound))
}

fn pocket(apron: &FireApronPreview, seat: usize) -> Vec3 {
    apron.pocket + apron.bay.dir * if seat == 0 { -0.6 } else { 0.6 }
}

fn at(sim: &Simulation, id: HumanID, position: Vec3) -> bool {
    sim.get(id).is_some_and(|human| {
        human.location == Location::Outside && human.trans.pos.is_close(position, 0.15)
    })
}

fn walk_to_pocket(sim: &mut Simulation, id: HumanID, apron: &FireApronPreview, seat: usize) {
    let position = pocket(apron, seat);
    let mut parking = sim.resources.write::<ParkingManagement>();
    let mut grid = sim.resources.write::<TransportGrid>();
    sim.world.edit_human(id, |human| {
        human.router.cancel_route(&mut parking);
        human.decision = Default::default();
        if matches!(human.location, Location::Building(_)) {
            human.location = Location::Outside;
            human.trans.pos = apron.door;
            human.collider = Some(put_pedestrian_in_transport_grid(&mut grid, apron.door));
        }
        human.it = Itinerary::simple(vec![position]);
    });
}

fn board(sim: &mut Simulation, truck: VehicleID, id: HumanID) {
    let mut grid = sim.resources.write::<TransportGrid>();
    sim.world.edit_human(id, |human| {
        if let Some(collider) = human.collider.take() {
            grid.remove_maintain(collider.0);
        }
        human.speed.0 = 0.0;
        human.location = Location::Vehicle(truck);
        human.it = Itinerary::NONE;
    });
}

fn exit_truck(sim: &mut Simulation, id: HumanID, apron: &FireApronPreview, seat: usize) {
    let position = pocket(apron, seat);
    let mut grid = sim.resources.write::<TransportGrid>();
    sim.world.edit_human(id, |human| {
        human.trans.pos = position;
        human.location = Location::Outside;
        human.collider = Some(put_pedestrian_in_transport_grid(&mut grid, position));
        human.it = Itinerary::simple(vec![apron.door]);
    });
}

fn start_sweep(sim: &mut Simulation, truck: VehicleID, path: &[Vec3]) {
    let vehicle = &sim.world.vehicles[truck];
    let collider = vehicle.collider.unwrap_or_else(|| {
        put_vehicle_in_transport_grid(sim, VehicleKind::Truck.width(), vehicle.trans)
    });
    sim.resources
        .write::<TransportGrid>()
        .get_mut(collider.0)
        .unwrap()
        .1
        .height = vehicle.trans.pos.z;
    let vehicle = &mut sim.world.vehicles[truck];
    vehicle.collider = Some(collider);
    vehicle.vehicle.state = VehicleState::Driving;
    vehicle.it = Itinerary::service_sweep(path);
}

fn park_at(sim: &mut Simulation, truck: VehicleID, pose: Transform) {
    let collider = {
        let vehicle = &mut sim.world.vehicles[truck];
        vehicle.trans = pose;
        vehicle.speed.0 = 0.0;
        vehicle.it = Itinerary::NONE;
        vehicle.vehicle.state = VehicleState::ParkedAtDepot;
        vehicle.collider.take()
    };
    if let Some(collider) = collider {
        sim.resources
            .write::<TransportGrid>()
            .remove_maintain(collider.0);
    }
}

fn sweep_clear(sim: &Simulation, truck: VehicleID, path: &[Vec3]) -> bool {
    let radius = VehicleKind::Truck.width() * 0.5;
    let own = sim
        .get(truck)
        .and_then(|vehicle| vehicle.collider)
        .map(|collider| collider.0);
    let grid = sim.resources.read::<TransportGrid>();
    let mut work = 256;
    for &point in path {
        for (handle, position) in grid.query_around(point.xy(), radius + 4.5) {
            if work == 0 {
                return false;
            }
            work -= 1;
            if Some(handle) != own
                && grid.get(handle).is_some_and(|(_, state)| {
                    (state.height - point.z).abs() <= 1.25
                        && position.is_close(point.xy(), radius + state.radius)
                })
            {
                return false;
            }
        }
    }
    true
}

fn await_clear_sweep(
    sim: &Simulation,
    truck: VehicleID,
    path: &[Vec3],
    trip: &mut Trip,
    now: Tick,
) -> bool {
    if now < trip.retry_at {
        return false;
    }
    let clear = sweep_clear(sim, truck, path);
    if !clear {
        trip.retry_at = Tick(now.0 + 10);
    }
    clear
}

fn transition(trip: &mut Trip, now: Tick, phase: ServiceTripPhase) {
    trip.phase = phase;
    trip.phase_since = now;
    trip.last_progress = now;
    trip.retry_at = now;
    trip.hold_reported = false;
}

fn event(
    sim: &Simulation,
    station: BuildingID,
    trip: &Trip,
    now: Tick,
    kind: FireResponseEventKind,
) {
    sim.resources
        .write::<FireFleet>()
        .record(station, trip, now, kind);
}

pub(crate) fn advance_fire_trips(sim: &mut Simulation) {
    if sim.resources.read::<FireFleet>().active_stations.is_empty() {
        return;
    }
    let now = elapsed_tick(sim);
    let daytime = sim.resources.read::<GameTime>().daytime;
    let stations: Vec<_> = sim
        .resources
        .read::<FireFleet>()
        .active_stations
        .iter()
        .copied()
        .collect();
    for site in stations {
        let mut station = sim
            .resources
            .write::<FireFleet>()
            .stations
            .remove(&site)
            .expect("active station");
        let mut trip = station.trip.take().expect("active response");
        let finished = step_trip(sim, site, station.truck, &station.apron, &mut trip, now);
        let mut fleet = sim.resources.write::<FireFleet>();
        if (finished || trip.failed) && fleet.assignments.get(&trip.incident) == Some(&site) {
            fleet.assignments.remove(&trip.incident);
        }
        if finished {
            fleet.active_stations.remove(&site);
            fleet.outbound.remove(&station.truck);
            for id in trip.crew.into_iter().flatten() {
                fleet.deployed.remove(&id);
            }
            if trip.failed {
                station.recovered += 1;
            } else {
                station.returned += 1;
            }
            fleet.record(
                site,
                &trip,
                now,
                if trip.failed {
                    FireResponseEventKind::Recovered
                } else {
                    FireResponseEventKind::Returned
                },
            );
        } else {
            if trip.crew.iter().flatten().any(|id| {
                station
                    .roster
                    .iter()
                    .position(|seat| *seat == Some(*id))
                    .is_some_and(|seat| !shift(seat, &daytime).is_active(&daytime))
            }) {
                station.overtime_ticks += 1;
            }
            station.trip = Some(trip);
        }
        fleet.stations.insert(site, station);
    }
}

fn step_trip(
    sim: &mut Simulation,
    station: BuildingID,
    truck: VehicleID,
    apron: &FireApronPreview,
    trip: &mut Trip,
    now: Tick,
) -> bool {
    use ServiceTripPhase::*;

    for seat in &mut trip.crew {
        if seat.is_some_and(|id| sim.get(id).is_none()) {
            sim.resources
                .write::<FireFleet>()
                .deployed
                .remove(&seat.take().unwrap());
            trip.failed = true;
        }
    }
    let active = fire_incident(sim, trip.site).is_some_and(|fire| fire.id == trip.incident);
    if trip.phase == Outbound && (!active || trip.failed) {
        sim.resources.write::<FireFleet>().outbound.remove(&truck);
    }
    let Some(vehicle) = sim.get(truck) else {
        trip.failed = true;
        // Preserve the owned scene and station geometry for diagnosis.
        return false;
    };
    let crew = trip.crew;
    let positions = [
        vehicle.trans.pos,
        crew[0]
            .and_then(|id| sim.get(id))
            .map_or(Vec3::ZERO, |h| h.trans.pos),
        crew[1]
            .and_then(|id| sim.get(id))
            .map_or(Vec3::ZERO, |h| h.trans.pos),
    ];
    if positions
        .iter()
        .zip(trip.last_positions)
        .any(|(new, old)| !new.is_close(old, 0.15))
    {
        trip.last_positions = positions;
        trip.last_progress = now;
    }
    let stalled = now.0.saturating_sub(trip.last_progress.0) >= WATCHDOG;
    if now.0.saturating_sub(trip.last_progress.0) >= HOLD_DEADLINE && !trip.hold_reported {
        trip.hold_reported = true;
        trip.failed = true;
        sim.resources.write::<FireFleet>().outbound.remove(&truck);
        event(sim, station, trip, now, FireResponseEventKind::Held);
    }

    match trip.phase {
        Boarding => {
            if trip.failed || !active || stalled {
                for id in crew.into_iter().flatten() {
                    sim.world
                        .edit_human(id, |human| human.it = Itinerary::simple(vec![apron.door]));
                }
                trip.failed = true;
                release_scene(sim, trip);
                transition(trip, now, LeavingTruck);
            } else if crew
                .iter()
                .enumerate()
                .all(|(seat, id)| id.is_some_and(|id| at(sim, id, pocket(apron, seat))))
            {
                // Leave the current crew on the pocket until a swept merge is clear.
                if !await_clear_sweep(sim, truck, &apron.departure_sweep, trip, now) {
                    return false;
                }
                for id in crew.into_iter().flatten() {
                    board(sim, truck, id);
                }
                event(sim, station, trip, now, FireResponseEventKind::Boarded);
                start_sweep(sim, truck, &apron.departure_sweep[1..]);
                transition(trip, now, Merging);
            }
        }
        Merging | LeavingScene => {
            if !vehicle.it.has_ended(0.0) {
                return false;
            }
            let (from, to, outbound) = if trip.phase == Merging && active && !trip.failed {
                (apron.departure, trip.scene.arrival, true)
            } else {
                let from = if trip.phase == Merging {
                    apron.departure
                } else {
                    trip.scene.departure
                };
                (from, apron.arrival, false)
            };
            if !outbound {
                if trip.phase == Merging {
                    trip.inbound = None;
                }
                release_scene(sim, trip);
            }
            if route_from(sim, truck, trip, from, to, outbound, now) {
                transition(trip, now, if outbound { Outbound } else { Returning });
                if outbound {
                    if let [Some(first), Some(second)] = crew {
                        sim.resources
                            .write::<FireFleet>()
                            .outbound
                            .insert(truck, (trip.site, trip.incident, [first, second]));
                    }
                }
            }
        }
        Outbound | Returning => {
            let outbound = trip.phase == Outbound && active && !trip.failed;
            let target = if outbound { &trip.scene } else { apron };
            let edit = trip.graph != access_revision(&sim.map());
            let needs_route = vehicle.it.is_wait_for_reroute().is_some()
                || (trip.phase == Outbound && !outbound)
                || edit;
            if needs_route {
                let current = vehicle
                    .it
                    .get_travers()
                    .and_then(|traversal| match traversal.kind {
                        TraverseKind::Lane(lane) => current_anchor(&sim.map(), lane, positions[0]),
                        TraverseKind::Turn(_) => None,
                    })
                    .or_else(|| {
                        vehicle.it.is_wait_for_reroute().and_then(|_| {
                            let map = sim.map();
                            let lane = map.nearest_lane(
                                positions[0],
                                crate::map::LaneKind::Driving,
                                Some(2.0),
                            )?;
                            current_anchor(&map, lane, positions[0])
                        })
                    });
                let to = target.arrival;
                if !outbound {
                    sim.resources.write::<FireFleet>().outbound.remove(&truck);
                }
                if let Some(from) = current {
                    trip.outbound = None;
                    trip.inbound = None;
                    if route_from(sim, truck, trip, from, to, outbound, now) {
                        if !outbound {
                            release_scene(sim, trip);
                            transition(trip, now, Returning);
                        }
                    }
                    return false;
                }
                // Continue clearing a valid turn under the ordinary junction controller.
                if !sim
                    .get(truck)
                    .is_some_and(|vehicle| vehicle.it.has_ended(0.0))
                {
                    return false;
                }
            }
            let vehicle = &sim.world.vehicles[truck];
            if vehicle.it.has_ended(0.0)
                && vehicle.trans.pos.is_close(target.arrival_sweep[0], 0.35)
            {
                let sweep = target.arrival_sweep;
                if !await_clear_sweep(sim, truck, &sweep, trip, now) {
                    return false;
                }
                sim.resources.write::<FireFleet>().outbound.remove(&truck);
                start_sweep(sim, truck, &sweep[1..]);
                transition(trip, now, if outbound { Parking } else { Docking });
            }
        }
        Parking | Docking => {
            if !vehicle.it.has_ended(0.0) {
                return false;
            }
            let target = if trip.phase == Parking {
                &trip.scene
            } else {
                apron
            };
            if !vehicle.trans.pos.is_close(target.bay.pos, 0.15) {
                return false;
            }
            park_at(sim, truck, target.bay);
            for (seat, id) in crew.into_iter().enumerate() {
                if let Some(id) = id {
                    exit_truck(sim, id, target, seat);
                }
            }
            transition(
                trip,
                now,
                if trip.phase == Parking {
                    WalkingToSite
                } else {
                    LeavingTruck
                },
            );
        }
        WalkingToSite => {
            if !active || trip.failed {
                walk_back(sim, trip, now);
            } else if crew
                .into_iter()
                .flatten()
                .all(|id| at(sim, id, trip.scene.door))
            {
                transition(trip, now, Working);
                event(sim, station, trip, now, FireResponseEventKind::Arrived);
            }
        }
        Working => {
            if !active || trip.failed {
                if !active
                    && !trip.failed
                    && now.0.saturating_sub(trip.phase_since.0)
                        >= SUPPRESSION_MINUTES * TICKS_PER_MINUTE
                {
                    trip.suppressed = true;
                    event(sim, station, trip, now, FireResponseEventKind::Suppressed);
                }
                walk_back(sim, trip, now);
            }
        }
        WalkingToTruck => {
            if crew
                .iter()
                .enumerate()
                .all(|(seat, id)| id.is_none_or(|id| at(sim, id, pocket(&trip.scene, seat))))
            {
                let sweep = trip.scene.departure_sweep;
                if !await_clear_sweep(sim, truck, &sweep, trip, now) {
                    return false;
                }
                for id in crew.into_iter().flatten() {
                    board(sim, truck, id);
                }
                start_sweep(sim, truck, &sweep[1..]);
                transition(trip, now, LeavingScene);
            }
        }
        LeavingTruck => {
            if crew.into_iter().flatten().all(|id| at(sim, id, apron.door)) {
                let still_open = sim
                    .map()
                    .get(station)
                    .is_some_and(|site| is_fire_station(site.kind));
                if still_open {
                    for id in crew.into_iter().flatten() {
                        let mut grid = sim.resources.write::<TransportGrid>();
                        sim.world.edit_human(id, |human| {
                            if let Some(collider) = human.collider.take() {
                                grid.remove_maintain(collider.0);
                            }
                            human.location = Location::Building(station);
                            human.it = Itinerary::NONE;
                        });
                    }
                }
                return true;
            }
        }
    }
    false
}

fn walk_back(sim: &mut Simulation, trip: &mut Trip, now: Tick) {
    for (seat, id) in trip.crew.into_iter().enumerate() {
        if let Some(id) = id {
            walk_to_pocket(sim, id, &trip.scene, seat);
        }
    }
    transition(trip, now, ServiceTripPhase::WalkingToTruck);
}

fn release_scene(sim: &Simulation, trip: &mut Trip) {
    if trip.scene_reserved {
        trip.scene_reserved = false;
        release_frontage(sim, trip.site, &trip.scene);
        sim.resources
            .write::<ParkingManagement>()
            .release_service_curb(&trip.scene.parking_displacements);
    }
}

fn route_from(
    sim: &mut Simulation,
    truck: VehicleID,
    trip: &mut Trip,
    from: DirectedLaneAnchor,
    to: DirectedLaneAnchor,
    outbound: bool,
    now: Tick,
) -> bool {
    if now < trip.retry_at {
        return false;
    }
    let map = sim.map();
    let revision = access_revision(&map);
    let stored = if outbound {
        trip.outbound.take()
    } else {
        trip.inbound.take()
    };
    let plan = stored.filter(|_| trip.graph == revision).or_else(|| {
        drive_route(&map, from, to, &mut AccessBudget(APRON_WORK))
            .ok()
            .map(|(_, route)| route)
    });
    let itinerary = plan.and_then(|route| Itinerary::service_drive(&map, from, to, route));
    drop(map);
    let vehicle = &mut sim.world.vehicles[truck];
    if let Some(itinerary) = itinerary {
        vehicle.it = itinerary;
        trip.graph = revision;
        true
    } else {
        // Keep the current directed traversal for the next metered retry.
        vehicle.speed.0 = 0.0;
        trip.retry_at = Tick(now.0 + RETRY_TICKS);
        false
    }
}

/// Rebuild transient movement indexes from the saved claims and verify their live counterparts.
pub(crate) fn restore_fire_fleet(sim: &Simulation) -> Result<(), &'static str> {
    let mut frontages = BTreeMap::new();
    let now = elapsed_tick(sim);
    let map = sim.map();
    let parking = sim.resources.read::<ParkingManagement>();
    let revision = access_revision(&map);
    let mut fleet = sim.resources.write::<FireFleet>();
    let mut trucks = BTreeSet::new();
    let mut parking_claims = BTreeSet::new();
    let mut crew = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut outbound = BTreeMap::new();
    let mut assignments = BTreeMap::new();
    let mut check_apron = |site, apron: &FireApronPreview| {
        if map.get(apron.road).is_none()
            || [apron.arrival, apron.departure].into_iter().any(|anchor| {
                map.get(anchor.lane).is_none_or(|lane| {
                    lane.kind != crate::map::LaneKind::Driving
                        || lane.parent != apron.road
                        || anchor.offset_cm as f32 / 100.0 > lane.points.length()
                })
            })
            || apron.arrival.lane != apron.departure.lane
            || apron.arrival.offset_cm >= apron.departure.offset_cm
            || !apron.bay.pos.is_finite()
            || !apron.bay.dir.is_finite()
            || !apron.pocket.is_finite()
            || !apron.door.is_finite()
            || apron
                .envelope
                .corners
                .iter()
                .any(|corner| !corner.is_finite())
            || apron
                .arrival_sweep
                .iter()
                .chain(&apron.departure_sweep)
                .any(|pos| !pos.is_finite())
            || apron.parking_displacements.iter().any(|&spot| {
                map.parking.get(spot).is_none_or(|p| {
                    map.get(p.parent)
                        .is_none_or(|lane| lane.parent != apron.road)
                }) || parking.is_spot_free(spot)
                    || !parking_claims.insert(spot)
            })
        {
            return Err("invalid persisted fire apron");
        }
        frontages
            .entry(apron.road)
            .or_insert_with(Vec::new)
            .push((site, apron.envelope));
        Ok(())
    };
    for (&site, station) in &mut fleet.stations {
        let building = map.get(site).ok_or("missing fire station")?;
        let vehicle = sim.get(station.truck).ok_or("missing fire truck")?;
        if (!is_fire_station(building.kind)
            && building.kind != crate::map::BuildingKind::SiteRemnant)
            || building.connected_road != Some(station.apron.road)
            || !trucks.insert(station.truck)
            || !matches!(vehicle.vehicle.kind, VehicleKind::Truck)
            || station
                .roster
                .iter()
                .flatten()
                .collect::<BTreeSet<_>>()
                .len()
                != station.roster.iter().flatten().count()
        {
            return Err("invalid persisted fire station");
        }
        check_apron(site, &station.apron)?;
        let Some(trip) = &mut station.trip else {
            if !matches!(vehicle.vehicle.state, VehicleState::ParkedAtDepot)
                || vehicle.trans != station.apron.bay
                || vehicle.collider.is_some()
            {
                return Err("idle fire truck is outside its apron");
            }
            continue;
        };
        active.insert(site);
        if trip.incident == 0
            || trip.phase_since > now
            || trip.last_progress > now
            || !trip.position.is_finite()
            || trip.last_positions.iter().any(|pos| !pos.is_finite())
            || trip.scene_reserved && map.get(trip.site).is_none()
            || !trip.failed && assignments.insert(trip.incident, site).is_some()
            || trip.crew.iter().flatten().any(|&id| {
                !crew.insert(id)
                    || sim.get(id).is_none_or(|human| match trip.phase {
                        ServiceTripPhase::Merging
                        | ServiceTripPhase::Outbound
                        | ServiceTripPhase::Parking
                        | ServiceTripPhase::LeavingScene
                        | ServiceTripPhase::Returning
                        | ServiceTripPhase::Docking => {
                            human.location != Location::Vehicle(station.truck)
                        }
                        _ => human.location != Location::Outside,
                    })
            })
        {
            return Err("invalid persisted fire response");
        }
        if trip.scene_reserved {
            check_apron(trip.site, &trip.scene)?;
        }
        if trip.phase == ServiceTripPhase::Outbound && !trip.failed {
            let [Some(first), Some(second)] = trip.crew else {
                return Err("an outbound response is missing a crew member");
            };
            if fire_incident(sim, trip.site).is_some_and(|fire| fire.id == trip.incident) {
                outbound.insert(station.truck, (trip.site, trip.incident, [first, second]));
            }
        }
        trip.graph = revision;
    }
    fleet.deployed = crew;
    fleet.outbound = outbound;
    fleet.active_stations = active;
    fleet.trucks = trucks;
    fleet.assignments = assignments;
    drop(map);
    sim.map_mut().service_frontages = frontages;
    Ok(())
}

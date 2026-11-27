use std::collections::{BTreeMap, BTreeSet, VecDeque};

use common::saveload::{Bincode, Encoder};
use geom::{Intersect, Segment, Segment3, Shape, Transform, Vec3, AABB};
use prototypes::{GameTime, Money, RecTimeInterval, ServiceID, Tick, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};

use super::events::{elapsed_tick, fire_incident, ServiceIncidents};
use super::service_access::{
    check_apron, check_scene_apron, corridor_clear, current_anchor, is_fire_station, walking_corridor, AccessBudget, AccessError,
    DirectedLaneAnchor, DriveSearch, FireApronPreview, APRON_WORK, MAX_FIRE_ROUTE,
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
const DISPATCH_ROUTE_WORK: usize = 768;
const RECOVERY_ROUTE_WORK: usize = 768;
const CANDIDATE_ROUTE_WORK: usize = 192;
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
    graph: Option<(u64, u64)>,
    failed: bool,
    suppressed: bool,
    hold_reported: bool,
    retry_at: Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ServiceCurbOwner {
    Station(BuildingID),
    Scene(BuildingID),
}

// A capsule around the whole truck (including front and rear) bounds its body
// throughout the sweep, independent of heading between waypoints.
const SWEEP_RADIUS: f32 = 3.0;
const TRUCK_OVERHANG: f32 = 3.0;
const SWEEP_DECK: f32 = 1.25;

struct SweepClaim {
    path: [Vec3; 17],
    bounds: AABB,
    pocket: Option<Vec3>,
}

impl SweepClaim {
    fn new(mut path: [Vec3; 17], pocket: Option<Vec3>) -> Self {
        path[0] -= (path[1] - path[0]).normalize() * TRUCK_OVERHANG;
        path[16] += (path[16] - path[15]).normalize() * TRUCK_OVERHANG;
        let mut bounds = Segment::new(path[0].xy(), path[16].xy()).bbox();
        for point in path.iter().chain(pocket.iter()) {
            bounds = bounds.union(AABB::new_ll_ur(point.xy(), point.xy()));
        }
        Self { path, bounds: bounds.expand(SWEEP_RADIUS), pocket }
    }

    fn blocks(&self, from: Vec3, to: Vec3, radius: f32) -> bool {
        if !(self.bounds.expand(radius)).intersects(&Segment::new(from.xy(), to.xy())) {
            return false;
        }
        let movement = Segment::new(from.xy(), to.xy());
        let same_deck = |z: f32| (from.z - z).abs().min((to.z - z).abs()) <= SWEEP_DECK;
        let hits = |segment: Segment, clearance: f32| {
            movement.distance(&segment) < clearance
                && (segment.project(from.xy()).distance2(from.xy()) >= clearance * clearance
                    || segment.project(to.xy()).distance2(to.xy())
                        <= segment.project(from.xy()).distance2(from.xy()))
        };
        self.path.windows(2).any(|pair| {
            same_deck(pair[0].z) &&
                hits(Segment::new(pair[0].xy(), pair[1].xy()), SWEEP_RADIUS + radius)
        }) || self.pocket.is_some_and(|pocket| {
            same_deck(pocket.z) && hits(Segment::new(pocket.xy(), pocket.xy()), 1.1 + radius)
        })
    }
}

/// Opt-in wall-time samples for the scenario runner. These never affect simulation state.
#[derive(Default)]
pub struct FireWorkTiming {
    pub enabled: bool,
    pub movement: std::time::Duration,
    pub access_edits: std::time::Duration,
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
    #[serde(skip)]
    routes: RouteWork,
    #[serde(skip)]
    sweeps: BTreeMap<VehicleID, SweepClaim>,
}

type AlarmKey = (Tick, BuildingID, u64);

#[derive(Default, Serialize, Deserialize)]
struct RouteWork {
    #[serde(skip)]
    graph: Option<(u64, u64)>,
    current: BTreeSet<BuildingID>,
    #[serde(deserialize_with = "decode_dispatch_searches")]
    dispatch: BTreeMap<AlarmKey, DispatchSearch>,
    #[serde(deserialize_with = "decode_recovery_searches")]
    recoveries: BTreeMap<VehicleID, DriveSearch>,
}

#[derive(Serialize, Deserialize)]
struct DispatchSearch {
    scene: FireApronPreview,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, STATION_LIMIT>")]
    stations: Vec<BuildingID>,
    search: Option<DriveSearch>,
    outgoing: Option<(u64, Vec<Traversable>)>,
    best: Option<RouteOffer>,
}

#[derive(Serialize, Deserialize)]
struct RouteOffer {
    rank: (u64, BuildingID),
    outbound: Vec<Traversable>,
    inbound: Vec<Traversable>,
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

fn decode_dispatch_searches<'de, D: serde::Deserializer<'de>>(d: D)
    -> Result<BTreeMap<AlarmKey, DispatchSearch>, D::Error>
{
    decode_map(d, 4096)
}

fn decode_recovery_searches<'de, D: serde::Deserializer<'de>>(d: D)
    -> Result<BTreeMap<VehicleID, DriveSearch>, D::Error>
{
    decode_map(d, STATION_LIMIT)
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
            deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_FIRE_ROUTE>"
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
    pub(crate) fn save(sim: &Simulation, mut output: &mut dyn std::io::Write) -> std::io::Result<()> {
        let fleet = sim.read::<Self>();
        let revision = access_revision(&sim.map());
        Bincode::encode_writer(&*fleet, &mut output)?;
        let current: BTreeSet<_> = fleet.stations.iter().filter_map(|(&site, station)| {
            (station.trip.as_ref()?.graph == Some(revision)).then_some(site)
        }).collect();
        let empty = RouteWork::default();
        let routes = if fleet.routes.graph == Some(revision) { &fleet.routes } else { &empty };
        Bincode::encode_writer(&(&current, &routes.dispatch, &routes.recoveries), output)
    }

    pub(crate) fn load(mut input: &[u8]) -> std::io::Result<Self> {
        let mut fleet: Self = Bincode::decode_reader(&mut input)?;
        if !input.is_empty() {
            fleet.routes = Bincode::decode(input)?;
        }
        Ok(fleet)
    }

    fn sync_routes(&mut self, revision: (u64, u64)) {
        if self.routes.graph != Some(revision) {
            self.routes = RouteWork {
                graph: Some(revision),
                ..RouteWork::default()
            };
        }
    }

    pub(crate) fn has_sweeps(&self) -> bool {
        !self.sweeps.is_empty()
    }

    pub(crate) fn sweep_allows(
        &self,
        owner: crate::transportation::junction_admission::EntryOwner,
        from: Vec3,
        to: Vec3,
        radius: f32,
    ) -> bool {
        self.sweeps.iter().all(|(&truck, claim)| {
            owner == crate::transportation::junction_admission::EntryOwner::Vehicle(truck)
                || !claim.blocks(from, to, radius)
        })
    }

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

    pub(crate) fn suppression_started(
        &self,
        sim: &Simulation,
        site: BuildingID,
        incident: u64,
    ) -> Option<Tick> {
        let station = self.stations.get(self.assignments.get(&incident)?)?;
        let trip = station.trip.as_ref()?;
        (trip.site == site
            && trip.incident == incident
            && trip.phase == ServiceTripPhase::Working
            && !trip.failed
            && crew_at_scene(sim, trip))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FireResponseView {
    pub station: BuildingID,
    pub truck: VehicleID,
    pub phase: ServiceTripPhase,
    pub arrived: bool,
}

impl FireFleet {
    pub fn response(&self, site: BuildingID, incident: u64) -> Option<FireResponseView> {
        let &id = self.assignments.get(&incident)?;
        let station = self.stations.get(&id)?;
        let trip = station.trip.as_ref()?;
        (trip.site == site && trip.incident == incident && !trip.failed).then_some(FireResponseView {
            station: id,
            truck: station.truck,
            phase: trip.phase,
            arrived: trip.phase == ServiceTripPhase::Working || trip.suppressed,
        })
    }

    pub fn station_ids(&self) -> impl Iterator<Item = BuildingID> + '_ {
        self.stations.keys().copied()
    }
}

#[cfg(test)]
pub(crate) fn cached_route_is_current(sim: &Simulation, station: BuildingID) -> bool {
    let fleet = sim.read::<FireFleet>();
    fleet.stations[&station].trip.as_ref().unwrap().graph == Some(access_revision(&sim.map()))
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
    sim.map_mut().service_frontages.entry(apron.road).or_default().extend([
        (site, apron.envelope),
        (site, walking_corridor(apron.bay, apron.pocket, apron.door)),
    ]);
}

fn release_frontage(sim: &Simulation, site: BuildingID, apron: &FireApronPreview) {
    let mut map = sim.map_mut();
    map.active_service_frontages.remove(&(apron.road, site));
    let Some(frontages) = map.service_frontages.get_mut(&apron.road) else {
        return;
    };
    frontages.retain(|(id, _)| *id != site);
    if frontages.is_empty() {
        map.service_frontages.remove(&apron.road);
    }
}

/// Reanchor unaffected bays after an admitted edit and decommission idle bays whose physical
/// access was removed. The station building can commission again through the retrofit command.
pub(crate) fn reconcile_idle_aprons(sim: &mut Simulation) {
    if sim.read::<FireFleet>().stations.is_empty() {
        return;
    }
    let timer = sim.read::<FireWorkTiming>().enabled.then(std::time::Instant::now);
    let retire = {
        let map = sim.map();
        let mut fleet = sim.write::<FireFleet>();
        fleet.stations.iter_mut().filter_map(|(&site, station)| {
            if station.trip.is_some() {
                return None;
            }
            let apron = &mut station.apron;
            let valid = map.get(site).is_some_and(|site| site.connected_road == Some(apron.road))
                && apron.parking_displacements.iter().all(|&spot| map.parking.get(spot).is_some())
                && !map.spatial_map().query(apron.envelope, crate::map::ProjectFilter::ALL)
                    .any(|object| matches!(object,
                        crate::map::ProjectKind::Road(id) if id != apron.road)
                        || matches!(object, crate::map::ProjectKind::Intersection(_)
                            | crate::map::ProjectKind::Building(_)))
                && corridor_clear(&map, Some(site), walking_corridor(apron.bay, apron.pocket, apron.door));
            let reanchor = |lane, point| {
                let anchor = current_anchor(&map, lane, point)?;
                let lane = map.get(lane)?;
                (lane.kind == crate::map::LaneKind::Driving && lane.parent == apron.road
                    && lane.points.point_along(anchor.offset_cm as f32 / 100.0).is_close(point, 0.04))
                    .then_some(anchor)
            };
            if valid {
                if let (Some(arrival), Some(departure)) = (
                    reanchor(apron.arrival.lane, apron.arrival_sweep[0]),
                    reanchor(apron.departure.lane, *apron.departure_sweep.last().unwrap()),
                ) {
                    if arrival.offset_cm < departure.offset_cm {
                        apron.arrival = arrival;
                        apron.departure = departure;
                        return None;
                    }
                }
            }
            Some(site)
        }).collect::<Vec<_>>()
    };
    for site in retire {
        release_station(sim, site);
    }
    if let Some(start) = timer {
        sim.write::<FireWorkTiming>().access_edits += start.elapsed();
    }
}
pub(crate) fn commission(sim: &mut Simulation, site: BuildingID, apron: FireApronPreview) {
    assert!(sim
        .resources
        .write::<ParkingManagement>()
        .reserve_service_curb(ServiceCurbOwner::Station(site), &apron.parking_displacements));
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
            .release_service_curb(ServiceCurbOwner::Station(site), &station.apron.parking_displacements);
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
    let revision = access_revision(&sim.map());
    let next = {
        let incidents = sim.resources.read::<ServiceIncidents>();
        let mut fleet = sim.resources.write::<FireFleet>();
        fleet.sync_routes(revision);
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
        let FireFleet { alarms, routes, .. } = &mut *fleet;
        routes.dispatch.retain(|key, _| alarms.contains_key(key));
        let next = alarms.iter()
            .filter(|(_, alarm)| alarm.due <= now)
            .min_by_key(|(&key, alarm)| (alarm.due, key))
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
    let result = prepare_trip(sim, key);
    let (station_id, crew, scene, outbound, inbound) = match result {
        Ok(result) => result,
        Err(reason) => {
            sim.resources.write::<FireFleet>().alarms.insert(
                key,
                Alarm {
                    due: Tick(now.0 + if reason == FireResponseBlocker::Route(AccessError::WorkLimit) {
                        1
                    } else {
                        RETRY_TICKS
                    }),
                    blocker: Some(reason),
                },
            );
            return;
        }
    };
    if !sim
        .resources
        .write::<ParkingManagement>()
        .reserve_service_curb(ServiceCurbOwner::Scene(site), &scene.parking_displacements)
    {
        return;
    }
    guard_frontage(sim, site, &scene);
    let station_apron = commissioned_apron(sim, station_id).expect("selected station");
    sim.map_mut().active_service_frontages.extend([(scene.road, site), (station_apron.road, station_id)]);
    let graph = Some(revision);
    let position = fire_incident(sim, site)
        .expect("active dispatch incident")
        .position;
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

fn prepare_trip(sim: &Simulation, key: AlarmKey) -> Result<Assignment, FireResponseBlocker> {
    let (_, site, _) = key;
    let map = sim.map();
    let target = map.get(site).ok_or(FireResponseBlocker::SceneAccess(
        AccessError::MissingFrontage,
    ))?;
    let pending = sim.write::<FireFleet>().routes.dispatch.remove(&key);
    let mut search = match pending {
        Some(search) => search,
        None => {
            let fleet = sim.read::<FireFleet>();
            let mut stations = Vec::new();
            let mut blocker = FireResponseBlocker::Staffing;
            for (&id, station) in &fleet.stations {
                let Some(spec) = map
                    .get(id)
                    .and_then(|building| building.kind.as_goods_company())
                    .and_then(|id| prototypes::try_prototype(ServiceID::from(id)))
                    .and_then(|proto| proto.fire)
                else {
                    continue;
                };
                if !station.apron.door.is_close(target.door_pos, f32::from(spec.max_response_m)) {
                    continue;
                }
                match dispatch_crew(sim, &fleet, id, station) {
                    Ok(_) => stations.push(id),
                    Err(FireResponseBlocker::Power) => blocker = FireResponseBlocker::Power,
                    Err(_) => {}
                }
            }
            if stations.is_empty() {
                return Err(blocker);
            }
            drop(fleet);
            let scene = check_scene_apron(
                sim,
                target.connected_road,
                target.door_pos,
                &mut AccessBudget(APRON_WORK),
            ).map_err(FireResponseBlocker::SceneAccess)?;
            stations.reverse();
            DispatchSearch { scene, stations, best: None, search: None, outgoing: None }
        }
    };

    let fleet = sim.read::<FireFleet>();
    let result = search.advance(sim, &map, &fleet, &mut AccessBudget(DISPATCH_ROUTE_WORK));
    drop(fleet);
    if result == Err(AccessError::WorkLimit) {
        sim.write::<FireFleet>().routes.dispatch.insert(key, search);
        return Err(FireResponseBlocker::Route(AccessError::WorkLimit));
    }
    result.map_err(FireResponseBlocker::Route)?;
    let best = search.best.ok_or(FireResponseBlocker::Route(AccessError::NoReturnLoop))?;
    let id = best.rank.1;
    let fleet = sim.read::<FireFleet>();
    let station = fleet.stations.get(&id).ok_or(FireResponseBlocker::Staffing)?;
    let crew = dispatch_crew(sim, &fleet, id, station)?;
    if map.service_apron_overlaps(&search.scene.envelope) {
        return Err(FireResponseBlocker::SceneAccess(AccessError::OccupiedCurb));
    }
    Ok((id, crew, search.scene, best.outbound, best.inbound))
}

fn dispatch_crew(
    sim: &Simulation,
    fleet: &FireFleet,
    id: BuildingID,
    station: &Station,
) -> Result<[HumanID; 2], FireResponseBlocker> {
    if station.trip.is_some() || sim.get(station.truck).is_none() {
        return Err(FireResponseBlocker::Staffing);
    }
    let crew = available_crew(sim, id, station)?;
    if crew.iter().any(|id| fleet.deployed.contains(id)) {
        return Err(FireResponseBlocker::Staffing);
    }
    Ok(crew)
}

impl DispatchSearch {
    fn advance(
        &mut self,
        sim: &Simulation,
        map: &Map,
        fleet: &FireFleet,
        work: &mut AccessBudget,
    ) -> Result<(), AccessError> {
        if self.best.as_ref().is_some_and(|best| {
            let id = best.rank.1;
            fleet.stations.get(&id).is_none_or(|s| dispatch_crew(sim, fleet, id, s).is_err())
        }) {
            // Reconsider candidates pruned against the old winner on the next alarm attempt.
            return Err(AccessError::NoReturnLoop);
        }
        while let Some(&id) = self.stations.last() {
            if work.0 == 0 {
                return Err(AccessError::WorkLimit);
            }
            work.0 -= 1;
            let result = match fleet.stations.get(&id) {
                Some(station) if dispatch_crew(sim, fleet, id, station).is_ok() =>
                    self.advance_candidate(map, id, station, work),
                _ => Err(AccessError::NoReturnLoop),
            };
            if result == Err(AccessError::WorkLimit) {
                return result;
            }
            self.stations.pop();
            self.search = None;
            self.outgoing = None;
        }
        Ok(())
    }

    fn advance_candidate(
        &mut self,
        map: &Map,
        id: BuildingID,
        station: &Station,
        work: &mut AccessBudget,
    ) -> Result<(), AccessError> {
        if self.outgoing.is_none() {
            let search = self.search.get_or_insert_with(|| {
                DriveSearch::new(station.apron.departure, self.scene.arrival)
            });
            let (ms, path) = search.advance(map, work)?;
            if self.best.as_ref().is_some_and(|best| best.rank <= (ms, id)) {
                return Ok(());
            }
            self.outgoing = Some((ms, path));
            self.search = None;
        }
        let search = self.search.get_or_insert_with(|| {
            DriveSearch::new(self.scene.departure, station.apron.arrival)
        });
        let (_, inbound) = search.advance(map, work)?;
        let (ms, outbound) = self.outgoing.take().expect("outbound route checked");
        self.best = Some(RouteOffer { rank: (ms, id), outbound, inbound });
        Ok(())
    }
}

fn pocket(apron: &FireApronPreview, seat: usize) -> Vec3 {
    apron.pocket + apron.bay.dir * if seat == 0 { -0.6 } else { 0.6 }
}

fn at(sim: &Simulation, id: HumanID, position: Vec3) -> bool {
    sim.get(id).is_some_and(|human| {
        human.location == Location::Outside && human.trans.pos.is_close(position, 0.15)
    })
}

fn crew_at_scene(sim: &Simulation, trip: &Trip) -> bool {
    trip.crew.iter().all(|id| id.is_some_and(|id| at(sim, id, trip.scene.door)))
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

fn claim_sweep(
    sim: &Simulation,
    truck: VehicleID,
    path: [Vec3; 17],
    pocket: Option<Vec3>,
    trip: &mut Trip,
    now: Tick,
) -> bool {
    if now < trip.retry_at {
        return false;
    }
    let claim = SweepClaim::new(path, pocket);
    if sweep_clear(sim, truck, &claim, trip.crew) {
        sim.write::<FireFleet>().sweeps.insert(truck, claim);
        true
    } else {
        trip.retry_at = Tick(now.0 + 10);
        false
    }
}

fn sweep_clear(sim: &Simulation, truck: VehicleID, claim: &SweepClaim, crew: [Option<HumanID>; 2]) -> bool {
    let own = sim.get(truck).and_then(|vehicle| vehicle.collider).map(|c| c.0);
    let responders = crew.map(|id| id.and_then(|id| sim.get(id)?.collider).map(|c| c.0));
    let fleet = sim.read::<FireFleet>();
    if fleet.sweeps.values().any(|active| {
        claim.bounds.intersects(&active.bounds)
            && claim.path.windows(2).any(|pair| active.blocks(pair[0], pair[1], SWEEP_RADIUS))
    }) {
        return false;
    }
    let grid = sim.read::<TransportGrid>();
    let bounds = claim.bounds.expand(VehicleKind::Bus.width() * 0.5);
    for (work, (handle, position)) in grid.query_aabb(bounds.ll, bounds.ur).enumerate() {
        if work >= 256 {
            return false;
        }
        if Some(handle) == own || responders.contains(&Some(handle)) {
            continue;
        }
        if grid.get(handle).is_some_and(|(_, actor)| {
            let pos = position.z(actor.height);
            claim.path.windows(2).any(|pair| {
                let nearest = Segment3::new(pair[0], pair[1]).project(pos);
                (nearest.z - pos.z).abs() <= SWEEP_DECK
                    && nearest.xy().is_close(position, SWEEP_RADIUS + actor.radius)
            }) || claim.pocket.is_some_and(|pocket| {
                (pocket.z - pos.z).abs() <= SWEEP_DECK
                    && pocket.xy().is_close(position, 1.1 + actor.radius)
            })
        }) {
            return false;
        }
    }
    true
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
    let revision = access_revision(&sim.map());
    let (mut stations, mut recoveries) = {
        let mut fleet = sim.write::<FireFleet>();
        fleet.sync_routes(revision);
        (fleet.active_stations.iter().copied().collect::<Vec<_>>(),
            std::mem::take(&mut fleet.routes.recoveries))
    };
    let offset = (now.0 % stations.len() as u64) as usize;
    stations.rotate_left(offset);
    let mut work = AccessBudget(RECOVERY_ROUTE_WORK);
    for site in stations {
        let mut station = sim.write::<FireFleet>().stations.remove(&site).expect("active station");
        let mut trip = station.trip.take().expect("active response");
        let mut recovery = recoveries.remove(&station.truck);
        let allowance = work.0.min(CANDIDATE_ROUTE_WORK);
        let mut slice = AccessBudget(allowance);
        let finished = step_trip(sim, site, station.truck, &station.apron, &mut trip, now,
            &mut recovery, &mut slice);
        work.0 -= allowance - slice.0;
        if !finished {
            if let Some(recovery) = recovery {
                recoveries.insert(station.truck, recovery);
            }
        }
        let mut fleet = sim.resources.write::<FireFleet>();
        if (finished || trip.failed) && fleet.assignments.get(&trip.incident) == Some(&site) {
            fleet.assignments.remove(&trip.incident);
        }
        if finished {
            sim.map_mut().active_service_frontages.remove(&(station.apron.road, site));
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
    sim.write::<FireFleet>().routes.recoveries = recoveries;
}

fn step_trip(
    sim: &mut Simulation,
    station: BuildingID,
    truck: VehicleID,
    apron: &FireApronPreview,
    trip: &mut Trip,
    now: Tick,
    recovery: &mut Option<DriveSearch>,
    work: &mut AccessBudget,
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
                if !claim_sweep(sim, truck, apron.departure_sweep, None, trip, now) {
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
            if !outbound && trip.phase == Merging {
                trip.inbound = None;
                release_scene(sim, trip);
            }
            if route_from(sim, truck, trip, from, to, outbound, now, recovery, work) {
                sim.write::<FireFleet>().sweeps.remove(&truck);
                if !outbound {
                    release_scene(sim, trip);
                }
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
            let edit = trip.graph != Some(access_revision(&sim.map()));
            let needs_route = vehicle.it.is_wait_for_reroute().is_some()
                || (trip.phase == Outbound && !outbound)
                || edit;
            if needs_route {
                let current = recovery.as_ref().map(DriveSearch::departure)
                    .or_else(|| vehicle.it
                    .get_travers()
                    .and_then(|traversal| match traversal.kind {
                        TraverseKind::Lane(lane) => current_anchor(&sim.map(), lane, positions[0]),
                        TraverseKind::Turn(_) => None,
                    }))
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
                    if route_from(sim, truck, trip, from, to, outbound, now, recovery, work) {
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
                if !claim_sweep(sim, truck, sweep, Some(target.pocket), trip, now) {
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
            sim.write::<FireFleet>().sweeps.remove(&truck);
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
            } else if crew_at_scene(sim, trip) {
                transition(trip, now, Working);
                event(sim, station, trip, now, FireResponseEventKind::Arrived);
            }
        }
        Working => {
            trip.failed |= !crew_at_scene(sim, trip);
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
                if !claim_sweep(sim, truck, sweep, None, trip, now) {
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
            .release_service_curb(ServiceCurbOwner::Scene(trip.site), &trip.scene.parking_displacements);
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
    recovery: &mut Option<DriveSearch>,
    work: &mut AccessBudget,
) -> bool {
    if now < trip.retry_at {
        return false;
    }
    let map = sim.map();
    let revision = access_revision(&map);
    if trip.graph != Some(revision) {
        trip.outbound = None;
        trip.inbound = None;
    }
    let stored = if outbound { trip.outbound.take() } else { trip.inbound.take() };
    let plan = match stored {
        Some(route) => Ok(route),
        None => {
            if recovery.as_ref().is_none_or(|search| !search.matches(from, to)) {
                *recovery = Some(DriveSearch::new(from, to));
            }
            recovery.as_mut().unwrap().advance(&map, work).map(|(_, path)| path)
        }
    };
    let incomplete = plan.as_ref().err() == Some(&AccessError::WorkLimit);
    let itinerary = plan.ok().and_then(|route| Itinerary::service_drive(&map, from, to, route));
    drop(map);
    let vehicle = &mut sim.world.vehicles[truck];
    if !incomplete {
        *recovery = None;
    }
    if let Some(itinerary) = itinerary {
        vehicle.it = itinerary;
        trip.graph = Some(revision);
        true
    } else {
        if matches!(trip.phase, ServiceTripPhase::Outbound | ServiceTripPhase::Returning) {
            vehicle.it = Itinerary::wait_for_reroute_to_lane(from.lane, from.offset_cm as f32 / 100.0);
        }
        vehicle.speed.0 = 0.0;
        trip.retry_at = Tick(now.0 + if incomplete { 1 } else { RETRY_TICKS });
        false
    }
}

fn valid_saved_route(
    map: &Map,
    from: DirectedLaneAnchor,
    to: DirectedLaneAnchor,
    path: &[Traversable],
) -> bool {
    if path.is_empty() {
        return from.lane == to.lane && from.offset_cm <= to.offset_cm;
    }
    let mut lane = from.lane;
    let mut chunks = path.rchunks_exact(2);
    for pair in &mut chunks {
        let (TraverseKind::Lane(dst), TraverseKind::Turn(turn)) = (pair[0].kind, pair[1].kind) else {
            return false;
        };
        if turn.src != lane || turn.dst != dst || turn.bidirectional
            || pair.iter().any(|step| step.dir != crate::map::TraverseDirection::Forward || step.raw_points(map).is_none())
            || map.get(dst).is_none_or(|lane| lane.kind != crate::map::LaneKind::Driving)
        {
            return false;
        }
        lane = dst;
    }
    chunks.remainder().is_empty() && lane == to.lane
}

/// Rebuild transient movement indexes from the saved claims and verify their live counterparts.
pub(crate) fn restore_fire_fleet(sim: &Simulation) -> Result<(), &'static str> {
    let mut frontages = BTreeMap::new();
    let mut active_frontages = BTreeSet::new();
    let now = elapsed_tick(sim);
    let map = sim.map();
    let revision = access_revision(&map);
    let mut fleet = sim.resources.write::<FireFleet>();
    let current = std::mem::take(&mut fleet.routes.current);
    fleet.routes.graph = Some(revision);
    let mut trucks = BTreeSet::new();
    let mut parking_claims = BTreeMap::new();
    let mut crew = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut outbound = BTreeMap::new();
    let mut sweeps = BTreeMap::new();
    let mut assignments = BTreeMap::new();
    let mut scenes = BTreeSet::new();
    let mut check_apron = |site, owner, apron: &FireApronPreview| {
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
                }) || parking_claims.insert(spot, owner).is_some()
            })
        {
            return Err("invalid persisted fire apron");
        }
        frontages
            .entry(apron.road)
            .or_insert_with(Vec::new)
            .extend([
                (site, apron.envelope),
                (site, walking_corridor(apron.bay, apron.pocket, apron.door)),
            ]);
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
        check_apron(site, ServiceCurbOwner::Station(site), &station.apron)?;
        if building.door_pos != station.apron.door {
            return Err("fire apron does not reach the station door");
        }
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
        active_frontages.insert((station.apron.road, site));
        if trip.incident == 0
            || trip.phase_since > now
            || trip.last_progress > now
            || !trip.position.is_finite()
            || trip.last_positions.iter().any(|pos| !pos.is_finite())
            || trip.scene_reserved && map.get(trip.site).is_none_or(|scene| {
                scene.door_pos != trip.scene.door || scene.connected_road != Some(trip.scene.road)
            })
            || (!trip.failed && trip.crew.contains(&None))
            || (!trip.failed && trip.phase == ServiceTripPhase::Working && !crew_at_scene(sim, trip))
            || (trip.scene_reserved && !scenes.insert(trip.site))
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
        let parked = match trip.phase {
            ServiceTripPhase::Boarding | ServiceTripPhase::LeavingTruck => Some(station.apron.bay),
            ServiceTripPhase::WalkingToSite | ServiceTripPhase::Working
                | ServiceTripPhase::WalkingToTruck => Some(trip.scene.bay),
            _ => None,
        };
        if let Some(bay) = parked {
            if vehicle.trans != bay || vehicle.collider.is_some()
                || !matches!(vehicle.vehicle.state, VehicleState::ParkedAtDepot)
            {
                return Err("fire truck does not reciprocate the crew phase");
            }
        }
        if current.contains(&site) && (
            trip.outbound.as_ref().is_some_and(|route| !valid_saved_route(&map, station.apron.departure, trip.scene.arrival, route))
                || trip.inbound.as_ref().is_some_and(|route| !valid_saved_route(&map, trip.scene.departure, station.apron.arrival, route))
        ) {
            return Err("invalid cached fire route");
        }
        if trip.scene_reserved {
            active_frontages.insert((trip.scene.road, trip.site));
            check_apron(trip.site, ServiceCurbOwner::Scene(trip.site), &trip.scene)?;
        }
        if trip.phase == ServiceTripPhase::Outbound && !trip.failed {
            let [Some(first), Some(second)] = trip.crew else {
                return Err("an outbound response is missing a crew member");
            };
            if fire_incident(sim, trip.site).is_some_and(|fire| fire.id == trip.incident) {
                outbound.insert(station.truck, (trip.site, trip.incident, [first, second]));
            }
        }
        let sweep = match trip.phase {
            ServiceTripPhase::Merging => Some(SweepClaim::new(station.apron.departure_sweep, None)),
            ServiceTripPhase::Parking => Some(SweepClaim::new(trip.scene.arrival_sweep, Some(trip.scene.pocket))),
            ServiceTripPhase::LeavingScene => Some(SweepClaim::new(trip.scene.departure_sweep, None)),
            ServiceTripPhase::Docking => Some(SweepClaim::new(station.apron.arrival_sweep, Some(station.apron.pocket))),
            _ => None,
        };
        if let Some(sweep) = sweep {
            sweeps.insert(station.truck, sweep);
        }
        trip.graph = current.contains(&site).then_some(revision);
    }
    if !current.is_subset(&active)
        || fleet.routes.dispatch.keys().any(|key| !fleet.alarms.contains_key(key))
        || fleet.routes.recoveries.keys().any(|truck| !trucks.contains(truck))
    {
        return Err("orphaned fire route work");
    }
    if !trucks.is_empty() && sim.world().humans.iter().any(|(human, actor)| {
        let Location::Vehicle(vehicle) = actor.location else { return false };
        trucks.contains(&vehicle) && !crew.contains(&human)
    }) {
        return Err("fire truck has an unassigned occupant");
    }
    sim.write::<ParkingManagement>().restore_service_curb(parking_claims, sim.world())?;
    fleet.sweeps = sweeps;
    fleet.deployed = crew;
    fleet.outbound = outbound;
    fleet.active_stations = active;
    fleet.trucks = trucks;
    fleet.assignments = assignments;
    drop(map);
    let mut map = sim.map_mut();
    map.service_frontages = frontages;
    map.active_service_frontages = active_frontages;
    Ok(())
}

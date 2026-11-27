use std::collections::{BTreeMap, BTreeSet, VecDeque};

use geom::{Color, Vec2, Vec3, AABB};
use ordered_float::OrderedFloat;
use prototypes::{Money, Tick};
use serde::{Deserialize, Serialize};
use slotmapd::{new_key_type, HopSlotMap, Key};

use crate::economy::{Budget, Government};
use crate::map::{
    spend_water_work, ConnectionError, LaneID, LaneKind, Map, ProjectFilter, ProjectKind, Road,
    RoadID, WaterTopologyWork,
};
use crate::map_dynamic::itinerary::ItineraryTarget;
use crate::map_dynamic::{Destination, Itinerary, Router, RoutingStep};
use crate::transportation::Location;
use crate::utils::resources::Resources;
use crate::world_command::{Rejection, WorldCommand};
use crate::{HumanEnt, HumanID, ParCommandBuffer, Simulation, VehicleEnt, VehicleID, World};

mod journey;
mod rail;
mod service;

pub(crate) use journey::validate_journey_snapshot;
pub use journey::{JourneyStats, LineJourneyStats};
pub(crate) use rail::{
    forget_rail_rider, rail_train_dropped, validate_rail_snapshot, MAX_PASSENGER_STOCK,
};
pub use rail::{
    rail_service_update, PassengerRailLine, PassengerRailRun, RailPlatformSpec, RailSchedule,
    RailTransit,
};

#[cfg(test)]
pub(super) use service::tests::recovery_leg_fixture;
pub use service::{transit_board_update, transit_dispatch_update, BUS_CAPACITY};

pub(crate) const WAIT_NOT_STARTED: Tick = Tick(u64::MAX);
/// Waiting for a bus is bounded by bus movement, so real time (12 real minutes,
/// two game hours at the old 10× clock).
pub const GIVE_UP_TICKS: u64 = 12 * 60 * prototypes::TICKS_PER_REALTIME_SECOND;
const STOP_RECHECK_TICKS: u64 = 600;
const BERTH_REACH: f32 = 4.0;
const SUSPENSION_HISTORY_LIMIT: usize = 256;

new_key_type! {
    pub struct StopID;
    pub struct LineID;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitStop {
    pub id: StopID,
    pub pos: Vec3,
    pub dir: Vec2,
    pub walk_pos: Vec3,
    pub berth: Vec3,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitLine {
    pub id: LineID,
    pub stops: Vec<StopID>,
    pub target_vehicles: u8,
    pub fare: Money,
    pub color: Color,
    pub suspended: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusRun {
    pub line: LineID,
    pub next_stop_idx: usize,
    /// Elapsed tick when dwelling ends.
    pub dwell_until: Option<Tick>,
    /// Elapsed tick when the current leg began.
    pub leg_started: Tick,
    pub failures: u8,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct LineSuspension {
    pub seq: u64,
    pub tick: Tick,
    pub line: LineID,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Berth {
    lane: LaneID,
    offset: f32,
    pos: Vec3,
    dir: Vec3,
}

impl Berth {
    fn lane_offset(self) -> (LaneID, f32) {
        (self.lane, self.offset)
    }

    fn target(self) -> ItineraryTarget {
        ItineraryTarget::DirectedLane(self.lane, self.offset)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Leg {
    from: Option<StopID>,
    expected_ticks: u64,
    skipped: BTreeSet<StopID>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(from = "Vec<u64>", into = "Vec<u64>")]
struct LineTimes {
    learned: Vec<u64>,
    nominal: Vec<u64>,
}

impl LineTimes {
    fn new(nominal: Vec<u64>) -> Self {
        Self {
            learned: nominal.clone(),
            nominal,
        }
    }

    fn refresh(&mut self, nominal: Vec<u64>) {
        if self.learned.len() != nominal.len() {
            self.learned.clone_from(&nominal);
        } else {
            for ((learned, &old), &new) in self.learned.iter_mut().zip(&self.nominal).zip(&nominal)
            {
                if old > 0 && old != new {
                    let old = u128::from(old);
                    let scaled = (u128::from(*learned) * u128::from(new) + old / 2) / old;
                    *learned = u64::try_from(scaled.max(1)).unwrap_or(u64::MAX);
                }
            }
        }
        self.nominal = nominal;
    }
}

// Older saves hold only positive learned times. Zero tags vectors that carry
// the learned times followed by their nominal baselines.
const LINE_TIMES_WITH_NOMINAL: u64 = 0;

impl From<Vec<u64>> for LineTimes {
    fn from(mut ticks: Vec<u64>) -> Self {
        let nominal = if ticks.first() == Some(&LINE_TIMES_WITH_NOMINAL) {
            let nominal = ticks.split_off(ticks.len().div_ceil(2));
            ticks.remove(0);
            nominal
        } else {
            Vec::new()
        };
        Self {
            learned: ticks,
            nominal,
        }
    }
}

impl From<LineTimes> for Vec<u64> {
    fn from(times: LineTimes) -> Self {
        if times.nominal.is_empty() {
            return times.learned;
        }
        let mut ticks = Vec::with_capacity(1 + times.learned.len() + times.nominal.len());
        ticks.push(LINE_TIMES_WITH_NOMINAL);
        ticks.extend(times.learned);
        ticks.extend(times.nominal);
        ticks
    }
}

#[derive(Serialize, Deserialize)]
struct TripExclusions {
    destination: Destination,
    started: Option<Tick>,
    all_lines: bool,
    lines: BTreeSet<LineID>,
}

impl TripExclusions {
    fn matches(&self, router: &Router) -> bool {
        router.target_dest == Some(self.destination)
            && self
                .started
                .is_none_or(|tick| router.trip_start == Some(tick))
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Transit {
    pub stops: HopSlotMap<StopID, TransitStop>,
    pub lines: HopSlotMap<LineID, TransitLine>,
    pub runs: BTreeMap<VehicleID, BusRun>,
    /// Increment when stop or line data changes, including while paused.
    pub revision: u64,
    // Lane IDs are a cache of the geometry anchors and are rebuilt on map edits.
    berths: BTreeMap<StopID, Berth>,
    // Only stops using a removed or reclipped lane can lose their current berth.
    #[serde(skip)]
    stops_by_lane: BTreeMap<LaneID, BTreeSet<StopID>>,
    leg_times: BTreeMap<LineID, LineTimes>,
    legs: BTreeMap<VehicleID, Leg>,
    trip_exclusions: BTreeMap<HumanID, TripExclusions>,
    suspensions: VecDeque<LineSuspension>,
    suspension_seq: u64,
}

impl Transit {
    pub(crate) const FARE_WINDOW_TICKS: u64 = 9_000;

    pub fn bus_leg_origin(&self, vehicle: VehicleID) -> Option<StopID> {
        self.legs.get(&vehicle)?.from
    }

    pub(crate) fn rebuild_berth_index(&mut self) {
        self.stops_by_lane.clear();
        for (&stop, berth) in &self.berths {
            self.stops_by_lane
                .entry(berth.lane)
                .or_default()
                .insert(stop);
        }
    }

    /// Estimated interval between buses, in elapsed ticks.
    pub fn headway(&self, line: LineID) -> Option<u64> {
        let info = self.lines.get(line)?;
        if info.suspended || info.target_vehicles == 0 {
            return None;
        }

        let buses = self
            .runs
            .values()
            .filter(|run| run.line == line)
            .take(info.target_vehicles as usize)
            .count() as u64;
        let (times, dwell) = self.directed_leg_times(line)?;
        (buses > 0).then(|| {
            let loop_ticks = times.iter().sum::<u64>() + times.len() as u64 * dwell;
            loop_ticks.div_ceil(buses)
        })
    }

    /// Estimated elapsed ticks for the directed legs in stop order, and the stop dwell.
    pub fn directed_leg_times(&self, line: LineID) -> Option<(&[u64], u64)> {
        let count = self.lines.get(line)?.stops.len();
        let times = self
            .leg_times
            .get(&line)
            .map(|times| times.learned.as_slice())
            .filter(|times| times.len() == count)?;
        Some((times, service::DWELL_TICKS))
    }

    pub(super) fn recovery_leg_ticks(&self, bus: VehicleID, run: &BusRun) -> Option<u64> {
        let leg = self.legs.get(&bus)?;
        let stops = &self.lines.get(run.line)?.stops;
        let previous = if run.next_stop_idx == 0 {
            stops.last()
        } else {
            stops.get(run.next_stop_idx - 1)
        }?;
        (leg.from != Some(*previous) && leg.expected_ticks > 0).then_some(leg.expected_ticks)
    }

    /// Whether this line may be considered when (re)planning the human's current trip.
    pub fn allows_line_for_trip(&self, human: HumanID, router: &Router, line: LineID) -> bool {
        self.trip_exclusions.get(&human).is_none_or(|excluded| {
            !excluded.matches(router) || (!excluded.all_lines && !excluded.lines.contains(&line))
        })
    }

    /// Yields retained suspension events newer than the given sequence, oldest first.
    pub fn suspensions_since(&self, seq: u64) -> impl Iterator<Item = &LineSuspension> {
        self.suspensions.iter().filter(move |event| event.seq > seq)
    }

    fn suspend(&mut self, line: LineID, tick: Tick) {
        if self.lines[line].suspended {
            return;
        }
        self.lines[line].suspended = true;
        self.revision += 1;
        self.suspension_seq += 1;
        if self.suspensions.len() == SUSPENSION_HISTORY_LIMIT {
            self.suspensions.pop_front();
        }
        self.suspensions.push_back(LineSuspension {
            seq: self.suspension_seq,
            tick,
            line,
        });
        log::warn!("Transit line {line:?} suspended after three consecutive skipped stops");
    }

    fn berth(&self, map: &Map, stop: StopID) -> Option<Berth> {
        if let Some(berth) = self.berths.get(&stop) {
            return Some(*berth);
        }
        let stop = self.stops.get(stop)?;
        resolve_stop(map, stop.pos, stop.dir).map(|(_, berth)| berth)
    }
}

/// Clock for transit leg, dwell, waiting, and trip timestamps. SetGameTime only
/// shifts the calendar clock. This clock advances by one tick per simulation step.
pub fn elapsed_ticks(resources: &Resources) -> Tick {
    let calendar_tick = resources.tick();
    Tick(resources.read::<Budget>().elapsed_ticks(calendar_tick))
}

pub fn simulation_elapsed_ticks(sim: &Simulation) -> Tick {
    elapsed_ticks(&sim.resources)
}

pub(crate) fn is_command(command: &WorldCommand) -> bool {
    use WorldCommand::*;

    matches!(
        command,
        TransitAddStop { .. }
            | TransitAddRailPlatform { .. }
            | TransitCreateRailLine { .. }
            | TransitSetRailSchedule { .. }
            | TransitRemoveStop { .. }
            | TransitCreateLine { .. }
            | TransitEditLine { .. }
            | TransitRemoveLine { .. }
            | TransitSetVehicles { .. }
            | TransitSetFare { .. }
    )
}

pub fn validate(sim: &Simulation, command: &WorldCommand) -> Result<(), Rejection> {
    if is_command(command) {
        prepare_command(sim, command)?;
    }
    Ok(())
}

enum Prepared {
    Stop(Vec2, Vec3, Berth),
    Line(Vec<u64>),
    RailPlatform(rail::PlatformPose),
    RailLine(rail::RailLoop, usize),
    Edit,
}

fn prepare_command(sim: &Simulation, command: &WorldCommand) -> Result<Prepared, Rejection> {
    use WorldCommand::*;

    let transit = sim.read::<Transit>();
    let rail = sim.read::<RailTransit>();
    let prepared = match command {
        TransitAddRailPlatform { platform } => {
            if rail.platforms.len() == rail::MAX_RAIL_PLATFORMS {
                return Err(Rejection::RoadWorkLimit);
            }
            if rail.platforms.values().any(|bound| {
                bound.lane == platform.lane
                    && (bound.offset - platform.offset).abs() < bound.length.max(platform.length)
            }) {
                return Err(Rejection::InvalidConnection);
            }
            Prepared::RailPlatform(rail::platform_pose(&sim.map(), *platform)?)
        }
        TransitCreateRailLine {
            calls,
            consist,
            schedule,
        } => {
            if rail.lines.len() == rail::MAX_RAIL_LINES {
                return Err(Rejection::RoadWorkLimit);
            }
            if !schedule.valid() {
                return Err(Rejection::InvalidConnection);
            }
            let (seats, _, _) = rail::fleet_spec(consist).ok_or(Rejection::InvalidTrain)?;
            Prepared::RailLine(
                rail::validate_loop(&sim.map(), &rail.platforms, calls, consist)?,
                seats,
            )
        }
        TransitSetRailSchedule {
            line,
            expected_pattern,
            schedule,
        } => {
            let config = rail.lines.get(line).ok_or(Rejection::MissingReference)?;
            if !schedule.valid()
                || config.pattern_revision != *expected_pattern
                || config.is_retiring()
            {
                return Err(Rejection::InvalidConnection);
            }
            Prepared::Edit
        }
        TransitEditLine { line, stops } if rail.lines.contains_key(line) => {
            let config = &rail.lines[line];
            if config.is_retiring() || rail.runs.values().any(|run| run.line == *line) {
                return Err(Rejection::OccupiedTrack);
            }
            Prepared::RailLine(
                rail::validate_loop(&sim.map(), &rail.platforms, stops, &config.consist)?,
                config.capacity,
            )
        }
        TransitRemoveStop { stop } if rail.platforms.contains_key(stop) => {
            if transit.lines.values().any(|line| line.stops.contains(stop)) {
                return Err(Rejection::OccupiedTrack);
            }
            Prepared::Edit
        }
        TransitAddStop { pos, dir } => {
            if !pos.is_finite() {
                return Err(Rejection::InvalidConnection);
            }
            let dir = stop_direction(*dir).ok_or(Rejection::InvalidConnection)?;
            sim.map()
                .authorize_land_effects(crate::map::PreparedEffects::Point(pos.xy()))?;
            let (walk, berth) =
                resolve_stop_checked(&sim.map(), *pos, dir)?.ok_or(Rejection::MissingReference)?;
            Prepared::Stop(dir, walk, berth)
        }
        TransitRemoveStop { stop } => {
            let site = transit
                .stops
                .get(*stop)
                .ok_or(Rejection::MissingReference)?;
            let map = sim.map();
            if map.land_ownership().is_tiled() {
                let berth = transit
                    .berths
                    .get(stop)
                    .ok_or(Rejection::MissingReference)?;
                let lane = map.get(berth.lane).ok_or(Rejection::MissingReference)?;
                map.authorize_transit_site(
                    site.pos,
                    site.walk_pos,
                    berth.pos,
                    berth.dir.xy(),
                    lane.parent,
                )?;
            }
            Prepared::Edit
        }
        TransitEditLine { line, .. }
        | TransitRemoveLine { line }
        | TransitSetVehicles { line, .. }
        | TransitSetFare { line, .. }
            if !transit.lines.contains_key(*line) =>
        {
            return Err(Rejection::MissingReference);
        }
        TransitCreateLine { stops } | TransitEditLine { stops, .. } => {
            if !(2..=30).contains(&stops.len()) {
                return Err(Rejection::InvalidConnection);
            }
            let map = sim.map();
            let berths = stops
                .iter()
                .map(|id| {
                    let stop = transit.stops.get(*id).ok_or(Rejection::MissingReference)?;
                    resolve_stop_checked(&map, stop.pos, stop.dir)?
                        .map(|(_, berth)| berth)
                        .ok_or(Rejection::InvalidConnection)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Prepared::Line(
                line_times(&map, &berths)
                    .collect::<Option<Vec<_>>>()
                    .ok_or(Rejection::InvalidConnection)?,
            )
        }
        TransitSetVehicles { line, n }
            if *n > 10 || rail.lines.get(line).is_some_and(|line| line.is_retiring()) =>
        {
            return Err(Rejection::InvalidConnection);
        }
        TransitSetFare { fare, .. } if !(Money::ZERO..=Money::new_bucks(10)).contains(fare) => {
            return Err(Rejection::InvalidConnection);
        }
        _ => Prepared::Edit,
    };

    let cost = Government::checked_action_cost(command, sim).ok_or(Rejection::CostOverflow)?;
    let balance = sim.read::<Government>().money;
    if cost > Money::ZERO && balance < cost {
        return Err(Rejection::InsufficientFunds);
    }
    balance
        .inner()
        .checked_sub(cost.inner())
        .ok_or(Rejection::CostOverflow)?;
    Ok(prepared)
}

fn stop_direction(dir: Vec2) -> Option<Vec2> {
    let scale = dir.x.abs().max(dir.y.abs());
    (dir.is_finite() && scale > 0.0).then(|| (dir / scale).normalize())
}

fn resolve_stop(map: &Map, pos: Vec3, dir: Vec2) -> Option<(Vec3, Berth)> {
    resolve_stop_checked(map, pos, dir).ok().flatten()
}

/// Preview the same walking point and vehicle berth resolved for a placed stop.
pub fn preview_stop(
    sim: &Simulation,
    pos: Vec3,
    dir: Vec2,
) -> Result<(Vec3, Vec3, Vec2), Rejection> {
    let Prepared::Stop(_, walk, berth) =
        prepare_command(sim, &WorldCommand::TransitAddStop { pos, dir })?
    else {
        return Err(Rejection::InvalidConnection);
    };
    Ok((walk, berth.pos, berth.dir.xy()))
}

/// Report stops whose berth would disappear after the accepted road edits.
pub fn stops_removed_by_demolition(
    sim: &Simulation,
    edits: &[ProjectKind],
) -> Result<Vec<(StopID, Vec3)>, ConnectionError> {
    let transit = sim.read::<Transit>();
    if edits.is_empty() || transit.stops.is_empty() {
        return Ok(Vec::new());
    }
    let map = sim.map();
    let mut work = WaterTopologyWork::new();
    let clips = crate::map::demolition_road_clips(&map, edits, &mut work.geometry())?;
    let remaining = &mut work.remaining;
    let mut affected = BTreeSet::new();
    for &road in clips.keys() {
        for (lane, _) in map.roads()[road].lanes_iter() {
            if let Some(stops) = transit.stops_by_lane.get(&lane) {
                spend_water_work(remaining, stops.len())?;
                affected.extend(stops.iter().copied());
            }
        }
    }

    let mut removed = Vec::new();
    let mut roads = Vec::new();
    for id in affected {
        let Some(stop) = transit.stops.get(id) else {
            continue;
        };
        let bounds = AABB::centered(stop.pos.xy(), Vec2::splat(BERTH_REACH * 2.0));
        map.spatial_map()
            .reserve_construction_query(bounds, remaining)?;
        for road in stop_roads(&map, stop.pos) {
            let clip_points = clips
                .get(&road.id)
                .and_then(Option::as_ref)
                .map_or(0, |clip| clip.len());
            let units = (road.points().len() + clip_points).saturating_mul(road.n_lanes());
            spend_water_work(remaining, units)?;
            roads.push(road);
        }
        if resolve_stop_on_roads(&map, stop.pos, stop.dir, roads.drain(..), Some(&clips))
            .ok()
            .flatten()
            .is_none()
        {
            removed.push((id, stop.pos));
        }
    }
    Ok(removed)
}

fn resolve_stop_checked(
    map: &Map,
    pos: Vec3,
    dir: Vec2,
) -> Result<Option<(Vec3, Berth)>, crate::map::LandDenial> {
    resolve_stop_on_roads(map, pos, dir, stop_roads(map, pos), None)
}

fn stop_roads(map: &Map, pos: Vec3) -> impl Iterator<Item = &Road> {
    map.spatial_map()
        .query_around(pos.xy(), BERTH_REACH, ProjectFilter::ROAD)
        .filter_map(|project| match project {
            ProjectKind::Road(id) => map.get(id),
            _ => None,
        })
}

fn resolve_stop_on_roads<'a>(
    map: &Map,
    pos: Vec3,
    dir: Vec2,
    roads: impl Iterator<Item = &'a Road>,
    clips: Option<&BTreeMap<RoadID, Option<geom::PolyLine3>>>,
) -> Result<Option<(Vec3, Berth)>, crate::map::LandDenial> {
    roads
        .flat_map(|road| {
            [road.src, road.dst].into_iter().filter_map(move |src| {
                // Lane order runs from the center line toward the curb.
                let &(lane, _) = road
                    .outgoing_lanes_from(src)
                    .iter()
                    .rfind(|(_, kind)| *kind == LaneKind::Driving)?;
                Some((road, lane, road.sidewalks(src).outgoing?))
            })
        })
        .filter_map(|(road, id, sidewalk)| {
            let lane = map.get(id)?;
            let sidewalk = map.get(sidewalk)?;
            let rebased;
            let (lane, sidewalk) = if let Some(clip) = clips.and_then(|clips| clips.get(&road.id)) {
                let mut road = road.clone();
                road.interfaced_points = clip.as_ref()?.clone();
                rebased = [lane, sidewalk].map(|lane| {
                    let mut lane = lane.clone();
                    lane.gen_pos(&road);
                    lane
                });
                (&rebased[0], &rebased[1])
            } else {
                (lane, sidewalk)
            };
            if lane.points.len() < 2 || lane.points.length() < 30.0 {
                return None;
            }
            let (projected, segment, lane_dir) = lane.points.project_segment_dir(pos);
            let distance = pos.distance2(projected);
            if distance > BERTH_REACH * BERTH_REACH || dir.dot(lane_dir.xy()) <= 0.0 {
                return None;
            }
            let prefix = lane
                .points
                .segments()
                .take(segment - 1)
                .map(|s| s.src.distance(s.dst))
                .sum::<f32>();
            let offset = prefix + lane.points[segment - 1].distance(projected);
            if !(15.0..=lane.points.length() - 15.0).contains(&offset) {
                return None;
            }
            let walk = sidewalk.points.project_segment(pos).0;
            if walk.distance2(pos) > 10.0 * 10.0 {
                return None;
            }
            let (vehicle_pos, dir) = lane.points.point_dir_along(offset);
            let berth = Berth {
                lane: id,
                offset,
                pos: vehicle_pos,
                dir,
            };
            let resolved = map
                .authorize_transit_site(pos, walk, berth.pos, berth.dir.xy(), lane.parent)
                .map(|()| (walk, berth));
            Some(((resolved.is_err(), OrderedFloat(distance), id), resolved))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, resolved)| resolved)
        .transpose()
}

pub(crate) fn validate_saved_land_stops(sim: &Simulation) -> Result<(), String> {
    let map = sim.map();
    let transit = sim.read::<Transit>();
    let rail = sim.read::<RailTransit>();
    for (id, stop) in transit.stops.iter() {
        if rail.platforms.contains_key(&id) {
            continue;
        }
        let berth = transit
            .berths
            .get(&id)
            .ok_or("Missing saved transit berth")?;
        let lane = map.get(berth.lane).ok_or("Missing saved transit lane")?;
        map.authorize_transit_site(
            stop.pos,
            stop.walk_pos,
            berth.pos,
            berth.dir.xy(),
            lane.parent,
        )
        .map_err(|error| format!("Invalid saved transit stop: {error:?}"))?;
    }
    Ok(())
}

fn line_times<'a>(map: &'a Map, berths: &'a [Berth]) -> impl Iterator<Item = Option<u64>> + 'a {
    berths
        .iter()
        .zip(berths.iter().cycle().skip(1))
        .map(move |(from, to)| {
            // Use the same route-selection seed whenever estimating a leg.
            let it =
                Itinerary::route_between_lanes(Tick(0), map, from.lane_offset(), to.lane_offset())?;
            Some(service::route_ticks(map, &it, from.pos, *to))
        })
}

/// Applies a validated edit. The command dispatcher handles construction charges and replay.
pub fn apply(sim: &mut Simulation, command: &WorldCommand) -> Result<(), Rejection> {
    use WorldCommand::*;

    let prepared = prepare_command(sim, command)?;
    let (world, resources) = sim.world_res();
    let mut transit = resources.write::<Transit>();
    let now = elapsed_ticks(resources);
    let mut rail = resources.write::<RailTransit>();

    match (command, prepared) {
        (TransitAddRailPlatform { platform }, Prepared::RailPlatform(pose)) => {
            let id = transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos: pose.berth,
                dir: pose.direction.xy(),
                walk_pos: pose.sidewalk,
                berth: pose.berth,
            });
            rail.platforms.insert(id, *platform);
            rail.poses.insert(id, pose);
        }
        (
            TransitCreateRailLine {
                calls,
                consist,
                schedule,
            },
            Prepared::RailLine(times, capacity),
        ) => {
            let id = insert_line(&mut transit, calls.clone(), 1);
            rail.lines.insert(
                id,
                rail::new_line(*schedule, capacity, consist.clone(), times, now),
            );
        }
        (TransitSetRailSchedule { line, schedule, .. }, _) => {
            let booked = rail
                .runs
                .values()
                .filter(|run| run.line == *line)
                .filter_map(|run| run.slot)
                .max();
            rail.lines
                .get_mut(line)
                .unwrap()
                .set_schedule(*schedule, now, booked);
        }
        (TransitEditLine { line, stops }, Prepared::RailLine(times, _)) => {
            if transit.lines[*line].stops != *stops {
                resources.write::<JourneyStats>().pattern_changed(*line);
            }
            let config = rail.lines.get_mut(line).unwrap();
            config.pattern_revision += 1;
            config.install_loop(times);
            transit.lines[*line].stops.clone_from(stops);
            transit.lines[*line].suspended = false;
            rail.pending_lines.remove(line);
        }
        (TransitRemoveLine { line }, _) if rail.lines.contains_key(line) => {
            if rail.runs.values().any(|run| run.line == *line) {
                rail.lines.get_mut(line).unwrap().remove();
                transit.lines[*line].target_vehicles = 0;
            } else {
                rail.lines.remove(line);
                rail.pending_lines.remove(line);
                transit.lines.remove(*line);
                resources.write::<JourneyStats>().remove_line(*line);
            }
        }
        (TransitRemoveStop { stop }, _) if rail.platforms.remove(stop).is_some() => {
            rail.poses.remove(stop);
            transit.stops.remove(*stop);
        }
        (TransitAddStop { pos, .. }, Prepared::Stop(dir, walk_pos, berth)) => {
            let id = transit.stops.insert_with_key(|id| TransitStop {
                id,
                pos: *pos,
                dir,
                walk_pos,
                berth: berth.pos,
            });
            transit.berths.insert(id, berth);
            transit
                .stops_by_lane
                .entry(berth.lane)
                .or_default()
                .insert(id);
        }
        (TransitRemoveStop { stop }, _) => {
            let map = resources.read::<Map>();
            remove_stops(
                world,
                resources,
                &map,
                &mut transit,
                &BTreeSet::from([*stop]),
                now,
            );
        }
        (TransitCreateLine { stops }, Prepared::Line(times)) => {
            let id = insert_line(&mut transit, stops.clone(), 0);
            transit.leg_times.insert(id, LineTimes::new(times));
        }
        (TransitEditLine { line, stops }, Prepared::Line(times)) => {
            let map = resources.read::<Map>();
            if transit.lines[*line].stops == *stops {
                transit.leg_times.entry(*line).or_default().refresh(times);
            } else {
                resources.write::<JourneyStats>().pattern_changed(*line);
                transit.leg_times.insert(*line, LineTimes::new(times));
            }
            let removed = replace_stops(world, &map, &mut transit, *line, stops.clone(), now);
            transit.lines[*line].suspended = false;
            world.humans.edit_each(|id, human| {
                if matches!(human.router.cur_step(), Some(RoutingStep::WaitAtStop { line: waiting, .. }) if waiting == line)
                    && removed
                        .iter()
                        .any(|&stop| human.router.references_stop(stop))
                {
                    let pos = walkable_egress(&map, human.trans.pos);
                    detach_for_replan(&mut transit, id, human, pos, resources, Some(*line));
                }
            });
        }
        (TransitRemoveLine { line }, _) => {
            let map = resources.read::<Map>();
            remove_line(world, resources, &map, &mut transit, *line);
        }
        (TransitSetVehicles { line, n }, _) => {
            transit.lines[*line].target_vehicles = *n;
        }
        (TransitSetFare { line, fare }, _) => transit.lines[*line].fare = *fare,
        _ => return Err(Rejection::TransitNotImplemented),
    }
    transit.revision += 1;
    Ok(())
}

fn insert_line(transit: &mut Transit, stops: Vec<StopID>, target_vehicles: u8) -> LineID {
    transit.lines.insert_with_key(|id| TransitLine {
        id,
        stops,
        target_vehicles,
        fare: Money::ZERO,
        color: Color::hsv(
            (id.data().as_ffi() as u32 % 360) as f32 * 137.5 % 360.0,
            0.7,
            0.9,
            1.0,
        ),
        suspended: false,
    })
}

fn replace_stops(
    world: &mut World,
    map: &Map,
    transit: &mut Transit,
    line: LineID,
    new_stops: Vec<StopID>,
    now: Tick,
) -> BTreeSet<StopID> {
    let old = std::mem::replace(&mut transit.lines[line].stops, new_stops);
    let new = &transit.lines[line].stops;
    let removed: BTreeSet<_> = old
        .iter()
        .copied()
        .filter(|stop| !new.contains(stop))
        .collect();

    let mut retarget = Vec::new();
    for (&bus, run) in transit.runs.iter_mut().filter(|(_, run)| run.line == line) {
        let index = run.next_stop_idx;
        if new.get(index) == old.get(index) && new.get(index).is_some() {
            // Preserve the same occurrence when a loop visits this stop more than once.
        } else if let Some(next) = old
            .get(index)
            .and_then(|stop| new.iter().position(|s| s == stop))
        {
            run.next_stop_idx = next;
        } else {
            run.next_stop_idx = old
                .iter()
                .cycle()
                .skip(index)
                .take(old.len())
                .find_map(|stop| new.iter().position(|s| s == stop))
                .unwrap_or(0);
            run.dwell_until = None;
            retarget.push(bus);
        }
        run.failures = 0;
        if !removed.is_empty() {
            transit
                .legs
                .entry(bus)
                .or_default()
                .skipped
                .extend(&removed);
        }
    }
    for bus in retarget {
        if let Some(vehicle) = world.vehicles.get_mut(bus) {
            service::begin_leg(transit, map, bus, vehicle, now, None);
        }
    }
    removed
}

fn remove_stops(
    world: &mut World,
    resources: &Resources,
    map: &Map,
    transit: &mut Transit,
    removed: &BTreeSet<StopID>,
    now: Tick,
) {
    for stop in removed {
        transit.stops.remove(*stop);
        if let Some(berth) = transit.berths.remove(stop) {
            if let Some(stops) = transit.stops_by_lane.get_mut(&berth.lane) {
                stops.remove(stop);
                if stops.is_empty() {
                    transit.stops_by_lane.remove(&berth.lane);
                }
            }
        }
    }
    let updates: Vec<_> = transit
        .lines
        .iter()
        .filter(|(_, line)| line.stops.iter().any(|stop| removed.contains(stop)))
        .map(|(id, line)| {
            let kept: Vec<_> = line
                .stops
                .iter()
                .copied()
                .filter(|stop| !removed.contains(stop))
                .collect();
            (id, kept)
        })
        .collect();
    for (line, kept) in updates {
        if kept.len() < 2 {
            remove_line(world, resources, map, transit, line);
        } else {
            resources.write::<JourneyStats>().pattern_changed(line);
            replace_stops(world, map, transit, line, kept, now);
            refresh_line_times(map, transit, line);
        }
    }

    world.humans.edit_each(|id, human| {
        if human.location.is_transit()
            || !removed
                .iter()
                .any(|&stop| human.router.references_stop(stop))
        {
            return;
        }
        if let Some(RoutingStep::WaitAtStop { line, .. }) = human.router.cur_step() {
            let line = *line;
            let pos = walkable_egress(map, human.trans.pos);
            detach_for_replan(transit, id, human, pos, resources, Some(line));
        } else {
            human.router.defer_replan();
        }
    });
}

fn remove_line(
    world: &mut World,
    resources: &Resources,
    map: &Map,
    transit: &mut Transit,
    line: LineID,
) {
    transit.lines.remove(line);
    resources.write::<JourneyStats>().remove_line(line);
    transit.leg_times.remove(&line);
    let vehicles = resources.read::<ParCommandBuffer<VehicleEnt>>();
    for (&id, _) in transit.runs.iter().filter(|(_, run)| run.line == line) {
        vehicles.kill(id);
    }
    world.humans.edit_each(|id, human| {
        if matches!(human.router.cur_step(), Some(RoutingStep::WaitAtStop { line: waiting, .. }) if *waiting == line)
            && human.location == Location::Outside
        {
            let pos = walkable_egress(map, human.trans.pos);
            detach_for_replan(transit, id, human, pos, resources, Some(line));
        }
    });
}

fn refresh_line_times(map: &Map, transit: &mut Transit, line: LineID) {
    let berths: Vec<_> = transit.lines[line]
        .stops
        .iter()
        .filter_map(|&stop| transit.berth(map, stop))
        .collect();
    let nominal = line_times(map, &berths)
        .zip(berths.iter().zip(berths.iter().cycle().skip(1)))
        .map(|(time, (from, to))| {
            time.unwrap_or_else(|| service::direct_travel_ticks(from.pos, to.pos))
        })
        .collect();
    transit.leg_times.entry(line).or_default().refresh(nominal);
}

/// Returns whether transit took over cancellation. An onboard rider keeps the
/// ride and decision until alighting, even when the destination is demolished.
pub fn on_route_cancelled(human: &mut HumanEnt, resources: &Resources) -> bool {
    if human.location.is_transit() {
        if let Some(Destination::Building(id)) = human.router.target_dest {
            if resources.read::<Map>().get(id).is_none() {
                human.router.defer_replan();
            }
        }
        return true;
    }
    if matches!(
        human.router.cur_step(),
        Some(RoutingStep::WaitAtStop { .. })
    ) && human.collider.is_none()
    {
        Router::force_detach(human, human.trans.pos, resources);
    }
    false
}

fn walkable_egress(map: &Map, position: Vec3) -> Vec3 {
    map.nearest_lane(position, LaneKind::Walking, None)
        .and_then(|lane| map.get(lane))
        .map(|lane| lane.points.project_segment(position).0)
        .unwrap_or_else(|| {
            let z = map
                .environment()
                .height(position.xy())
                .unwrap_or(position.z);
            position.xy().z(z)
        })
}

fn detach_for_replan(
    transit: &mut Transit,
    id: HumanID,
    human: &mut HumanEnt,
    position: Vec3,
    resources: &Resources,
    excluded_line: Option<LineID>,
) {
    resources.write::<JourneyStats>().discard_plan(id);
    if let Some(destination) = human.router.target_dest {
        let started = human.router.trip_start;
        let exclusions = transit
            .trip_exclusions
            .entry(id)
            .or_insert_with(|| TripExclusions {
                destination,
                started,
                all_lines: false,
                lines: BTreeSet::new(),
            });
        if !exclusions.matches(&human.router) {
            *exclusions = TripExclusions {
                destination,
                started,
                all_lines: false,
                lines: BTreeSet::new(),
            };
        }
        match excluded_line {
            Some(line) => {
                exclusions.lines.insert(line);
            }
            None => exclusions.all_lines = true,
        }
    }
    Router::force_detach(human, position, resources);
}

pub fn on_vehicle_dropped(sim: &mut Simulation, vehicle: VehicleID, position: Vec3) {
    let (world, resources) = sim.world_res();
    let mut transit = resources.write::<Transit>();
    let Some(run) = transit.runs.remove(&vehicle) else {
        return;
    };
    transit.legs.remove(&vehicle);
    let mut egress = None;
    world.humans.edit_each(|id, human| {
        if human.location == Location::Transit(vehicle) {
            let egress =
                *egress.get_or_insert_with(|| walkable_egress(&resources.read::<Map>(), position));
            detach_for_replan(&mut transit, id, human, egress, resources, Some(run.line));
        }
    });
}

pub fn on_topology_changed(sim: &mut Simulation) {
    if sim.read::<Transit>().stops.is_empty() {
        return;
    }
    let (world, resources) = sim.world_res();
    resources.write::<JourneyStats>().topology_changed();
    revalidate_stops(world, resources, true);
    rail::revalidate_rail(resources);
    crate::utils::scheduler::drain_commands(sim);
}

pub fn transit_stop_update(world: &mut World, resources: &mut Resources) {
    if !resources.read::<Transit>().stops.is_empty()
        && elapsed_ticks(resources).0 % STOP_RECHECK_TICKS == 0
    {
        revalidate_stops(world, resources, false);
    }
}

fn revalidate_stops(world: &mut World, resources: &Resources, topology_edited: bool) {
    let mut transit = resources.write::<Transit>();
    if transit.stops.is_empty() {
        return;
    }
    let map = resources.read::<Map>();
    let now = elapsed_ticks(resources);
    let mut moved = BTreeSet::new();
    let mut removed = BTreeSet::new();
    let mut changed_lanes = false;
    let rail = resources.read::<RailTransit>();
    let Transit { stops, berths, .. } = &mut *transit;
    for (id, stop) in stops.iter_mut() {
        if rail.platforms.contains_key(&id) {
            continue;
        }
        let Some((walk, berth)) = resolve_stop(&map, stop.pos, stop.dir) else {
            removed.insert(id);
            continue;
        };
        let previous = berths.insert(id, berth);
        changed_lanes |= previous.is_none_or(|previous| previous.lane != berth.lane);
        if previous != Some(berth) || stop.walk_pos != walk || stop.berth != berth.pos {
            stop.walk_pos = walk;
            stop.berth = berth.pos;
            moved.insert(id);
        }
    }
    if !removed.is_empty() {
        remove_stops(world, resources, &map, &mut transit, &removed, now);
    }
    if changed_lanes {
        transit.rebuild_berth_index();
    }
    if !moved.is_empty() || !removed.is_empty() {
        transit.revision += 1;
    }
    if topology_edited || !moved.is_empty() {
        let lines: Vec<_> = transit
            .lines
            .iter()
            .filter(|(_, line)| {
                !rail.lines.contains_key(&line.id)
                    && (topology_edited || line.stops.iter().any(|stop| moved.contains(stop)))
            })
            .map(|(id, _)| id)
            .collect();
        for line in lines {
            if transit.lines[line].stops.iter().any(|stop| moved.contains(stop)) {
                resources.write::<JourneyStats>().pattern_changed(line);
            }
            refresh_line_times(&map, &mut transit, line);
        }
    }

    let mut reroute = Vec::new();
    for (&id, run) in &transit.runs {
        let Some(vehicle) = world.vehicles.get(id) else {
            continue;
        };
        let Some(&target) = transit
            .lines
            .get(run.line)
            .and_then(|line| line.stops.get(run.next_stop_idx))
        else {
            continue;
        };
        let bad_route = vehicle.it.get_route().is_some_and(|route| {
            route.cur.raw_points(&map).is_none()
                || route
                    .reversed_route
                    .iter()
                    .any(|traversal| traversal.raw_points(&map).is_none())
        });
        let moved_origin = transit
            .legs
            .get(&id)
            .and_then(|leg| leg.from)
            .is_some_and(|start| moved.contains(&start) || removed.contains(&start));
        let changed_target = vehicle
            .it
            .target()
            .zip(transit.berths.get(&target))
            .is_some_and(|(current, berth)| current != berth.target());
        if moved.contains(&target)
            || (run.dwell_until.is_none()
                && (moved_origin
                    || bad_route
                    || changed_target
                    || (topology_edited && vehicle.it.is_wait_for_reroute().is_some())))
        {
            reroute.push(id);
        }
    }
    for id in reroute {
        transit.runs.get_mut(&id).unwrap().dwell_until = None;
        service::begin_leg(&mut transit, &map, id, &mut world.vehicles[id], now, None);
    }
}

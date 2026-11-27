use std::collections::{BTreeMap, BTreeSet};

use geom::{Segment, Shape, Vec3};
use prototypes::{try_prototype, Money, RollingStockID, Tick, TICKS_PER_HOUR};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::economy::{Budget, BudgetCategory, Government, TICKS_PER_BUDGET_DAY};
use crate::map::{
    BuildingID, BuildingKind, LaneID, LaneKind, Map, ProjectFilter, ProjectKind, Traversable, TraverseKind,
};
use crate::map_dynamic::Itinerary;
use crate::transportation::train::{occupied_lanes, spawn_train, train_length, RailWagonKind};
use crate::transportation::Location;
use crate::world::{HumanID, TrainEnt, TrainID};
use crate::world_command::{PrepareBudget, Rejection};
use crate::{ParCommandBuffer, Simulation};

use super::{elapsed_ticks, BusRun, LineID, Router, StopID, Transit};

pub(crate) const MAX_PASSENGER_STOCK: usize = 8;
pub(super) const MAX_RAIL_PLATFORMS: usize = 1_024;
pub(super) const MAX_RAIL_LINES: usize = 256;
const MAX_RAIL_RUNS: usize = MAX_RAIL_LINES * 10;
const MAX_HELD_RIDERS: usize = 160_000;
const MAX_LOOP_TRAVERSALS: usize = 4096;
const MAX_CONNECTOR_M: f32 = 60.0;
const PLATFORM_HEIGHT_TOLERANCE: f32 = 2.5;
const BERTH_TOLERANCE: f32 = 1.0;
const STOPPED_SPEED: f32 = 0.25;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RailPlatformSpec {
    pub station: BuildingID,
    pub lane: LaneID,
    /// Directed metres from the lane entry to the stopping point for the lead.
    pub offset: f32,
    /// Usable platform extends this far behind the stopping point.
    pub length: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RailSchedule {
    pub anchor_elapsed: Tick,
    pub headway_ticks: u64,
    pub dwell_ticks: u64,
    pub max_late_ticks: u64,
}

impl Default for RailSchedule {
    fn default() -> Self {
        Self {
            anchor_elapsed: Tick(0),
            headway_ticks: 1_000,
            dwell_ticks: 100,
            max_late_ticks: 250,
        }
    }
}

impl RailSchedule {
    pub(super) fn valid(self) -> bool {
        (300..=6_000).contains(&self.headway_ticks)
            && (50..=600).contains(&self.dwell_ticks)
            && self.max_late_ticks < self.headway_ticks
            && self.max_late_ticks <= 600
    }

    pub(crate) fn slot_at_or_after(self, tick: u64) -> u64 {
        let n = tick
            .saturating_sub(self.anchor_elapsed.0)
            .div_ceil(self.headway_ticks);
        self.anchor_elapsed
            .0
            .saturating_add(n.saturating_mul(self.headway_ticks))
    }

    /// Earliest legal departure after a train can finish boarding.
    fn departure_in_slot(self, slot: u64, ready: u64) -> Option<Tick> {
        let departure = slot.max(ready);
        (departure <= slot.saturating_add(self.max_late_ticks)).then_some(Tick(departure))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PassengerRailLine {
    pub schedule: RailSchedule,
    pub pattern_revision: u64,
    pub capacity: usize,
    pub missed_slots: u64,
    daily_upkeep: Money,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_PASSENGER_STOCK>")]
    pub(crate) consist: Vec<RollingStockID>,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, 30>")]
    pub(crate) leg_ticks: Vec<u64>,
    pub(crate) next_slot: u64,
    removing: bool,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_LOOP_TRAVERSALS>")]
    paths: Vec<(Traversable, u64)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PassengerRailRun {
    pub line: LineID,
    pub(super) next_call: usize,
    pub(super) close_at: Option<Tick>,
    // A completed directed route has reached the measured berth. Clearing its
    // movement target lets the locomotive settle to zero before opening doors.
    braking_at_berth: bool,
    pub(super) leg_started: Tick,
    pub(super) call_sequence: u64,
    pub(crate) slot: Option<u64>,
}

impl PassengerRailRun {
    pub fn next_platform_index(&self) -> usize {
        self.next_call
    }

    pub fn dwell_until(&self) -> Option<Tick> {
        self.close_at
    }

    pub(crate) fn forecast(&self, config: &PassengerRailLine, now: Tick) -> BusRun {
        let mut close = self.close_at;
        if self.next_call == 0 && self.slot.is_none() {
            let schedule = config.schedule;
            let ready = close.or_else(|| {
                let remaining = config.leg_ticks.last()?
                    .checked_sub(now.0.saturating_sub(self.leg_started.0))?;
                Some(Tick(now.0.saturating_add(remaining).saturating_add(schedule.dwell_ticks)))
            });
            close = ready.and_then(|ready| {
                let ready = ready.0.max(now.0.saturating_add(1));
                let slot = config.next_slot.max(
                    schedule.slot_at_or_after(ready.saturating_sub(schedule.max_late_ticks))
                );
                schedule.departure_in_slot(slot, ready)
            });
        }
        BusRun {
            line: self.line,
            next_stop_idx: self.next_call,
            dwell_until: close,
            leg_started: self.leg_started,
            failures: 0,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct RailTransit {
    #[serde(deserialize_with = "bounded_map::<_, _, _, MAX_RAIL_PLATFORMS>")]
    pub platforms: BTreeMap<StopID, RailPlatformSpec>,
    #[serde(deserialize_with = "bounded_map::<_, _, _, MAX_RAIL_LINES>")]
    pub lines: BTreeMap<LineID, PassengerRailLine>,
    #[serde(deserialize_with = "bounded_map::<_, _, _, MAX_RAIL_RUNS>")]
    pub runs: BTreeMap<TrainID, PassengerRailRun>,
    #[serde(deserialize_with = "bounded_map::<_, _, _, MAX_HELD_RIDERS>")]
    held: BTreeMap<HumanID, HeldRider>,
    #[serde(skip)]
    riders: BTreeMap<TrainID, BTreeSet<HumanID>>,
    #[serde(skip)]
    pub(super) poses: BTreeMap<StopID, PlatformPose>,
    upkeep_pending: i128,
    #[serde(deserialize_with = "bounded_map::<_, _, _, MAX_RAIL_LINES>")]
    pub(super) pending_lines: BTreeMap<LineID, ()>,
}

pub(in crate::map_dynamic) fn bounded_map<'de, D, K, V, const LIMIT: usize>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Ord + Deserialize<'de>,
    V: Deserialize<'de>,
{
    struct Bounded<K, V, const LIMIT: usize>(std::marker::PhantomData<(K, V)>);

    impl<'de, K, V, const LIMIT: usize> Visitor<'de> for Bounded<K, V, LIMIT>
    where
        K: Ord + Deserialize<'de>,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a passenger map with at most {LIMIT} entries")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
            if input.size_hint().is_some_and(|len| len > LIMIT) {
                return Err(A::Error::custom("passenger map exceeds its limit"));
            }
            let mut entries = BTreeMap::new();
            while let Some(key) = input.next_key()? {
                if entries.len() == LIMIT {
                    return Err(A::Error::custom("passenger map exceeds its limit"));
                }
                if entries.insert(key, input.next_value()?).is_some() {
                    return Err(A::Error::custom("duplicate passenger map key"));
                }
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Bounded::<K, V, LIMIT>(std::marker::PhantomData))
}

#[derive(Serialize, Deserialize)]
struct HeldRider {
    position: Vec3,
    lane: Option<LaneID>,
    deadline: Tick,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct PlatformPose {
    pub sidewalk: Vec3,
    pub door: Vec3,
    pub boarding: Vec3,
    pub berth: Vec3,
    pub direction: Vec3,
}

pub(super) fn platform_pose(map: &Map, spec: RailPlatformSpec) -> Result<PlatformPose, Rejection> {
    let station = map.get(spec.station).ok_or(Rejection::MissingReference)?;
    let track = map.get(spec.lane).ok_or(Rejection::MissingReference)?;
    let sidewalk = station
        .connected_road
        .and_then(|road| map.get(road))
        .into_iter()
        .flat_map(|road| road.lanes_iter())
        .filter(|&(_, kind)| kind == LaneKind::Walking)
        .filter_map(|(lane, _)| {
            let pos = map.get(lane)?.points.project(station.door_pos);
            Some((ordered_float::OrderedFloat(pos.distance2(station.door_pos)), lane, pos))
        })
        .min_by_key(|&(distance, lane, _)| (distance, lane))
        .map(|(_, _, pos)| pos);
    if station.kind != BuildingKind::TrainStation
        || track.kind != LaneKind::Rail
        || !spec.offset.is_finite()
        || !spec.length.is_finite()
        || !(10.0..=250.0).contains(&spec.length)
        || spec.offset < spec.length
        || spec.offset + BERTH_TOLERANCE > track.points.length()
    {
        return Err(Rejection::InvalidConnection);
    }
    let (berth, direction) = track.points.point_dir_along(spec.offset);
    let door = station.door_pos;
    let sidewalk = sidewalk.ok_or(Rejection::InvalidConnection)?;
    let normal = direction.xy().normalize().perpendicular();
    let side = normal.dot(door.xy() - berth.xy());
    let setback = 0.5 * LaneKind::Rail.width() + 1.0;
    let boarding = berth + (normal * (setback * side.signum())).z0();
    if door.distance(berth) > MAX_CONNECTOR_M
        || sidewalk.distance(door) > MAX_CONNECTOR_M
        || side.abs() < setback
        || !dry_walk_connector(map, sidewalk, door)
        || !dry_walk_connector(map, door, boarding)
    {
        return Err(Rejection::InvalidConnection);
    }

    // Sample each clipped segment of the directed deck, including every bend.
    let deck = track
        .points
        .cut(spec.offset - spec.length, track.points.length() - spec.offset);
    for segment in deck.segments() {
        if !segment_samples(segment.src, segment.dst).all(|point| {
            dry(map, point) && (point.z - boarding.z).abs() <= PLATFORM_HEIGHT_TOLERANCE
        }) {
            return Err(Rejection::InvalidConnection);
        }
        map.authorize_transit_site(
            segment.src,
            segment.src,
            segment.dst,
            direction.xy(),
            track.parent,
        )?;
    }
    map.authorize_transit_site(door, door, berth, direction.xy(), track.parent)?;
    Ok(PlatformPose {
        sidewalk,
        door,
        boarding,
        berth,
        direction,
    })
}

fn segment_samples(from: Vec3, to: Vec3) -> impl Iterator<Item = Vec3> {
    let samples = (from.distance(to) / 0.75).ceil().max(1.0) as u32;
    (0..=samples).map(move |sample| from + (to - from) * (sample as f32 / samples as f32))
}

fn dry(map: &Map, point: Vec3) -> bool {
    point.is_finite()
        && map.environment().water_surface_elevation(point.xy())
            .is_none_or(|height| point.z > height)
}

fn dry_walk_connector(map: &Map, from: Vec3, to: Vec3) -> bool {
    if (to.z - from.z).abs() > 0.25 * to.xy().distance(from.xy()) + 0.25
        || !segment_samples(from, to).all(|point| {
            dry(map, point)
                && map.environment().height(point.xy())
                    .is_some_and(|height| (height - point.z).abs() <= 1.0)
        })
    {
        return false;
    }
    let bounds = Segment::new(from.xy(), to.xy()).bbox().expand(4.0);
    for road in map.spatial_map.query(bounds, ProjectFilter::ROAD) {
        let ProjectKind::Road(road) = road else {
            continue;
        };
        for (lane, kind) in map.get(road).into_iter().flat_map(|road| road.lanes_iter()) {
            if kind == LaneKind::Walking {
                continue;
            }
            let Some(lane) = map.get(lane) else {
                continue;
            };
            if segment_samples(from, to).any(|point| {
                let near = lane.points.project_2d(point.xy());
                (near.z - point.z).abs() < PLATFORM_HEIGHT_TOLERANCE
                    && near.xy().distance2(point.xy()) <= (0.5 * kind.width() + 0.25).powi(2)
            }) {
                return false;
            }
        }
    }
    true
}

pub(super) fn fleet_spec(consist: &[RollingStockID]) -> Option<(usize, Money, Money)> {
    if !(1..=MAX_PASSENGER_STOCK).contains(&consist.len()) {
        return None;
    }
    let mut seats = 0usize;
    let mut price = 0i64;
    let mut upkeep = 0i64;
    let mut mass = 0u32;
    let mut acceleration = 0.0;
    let mut braking = 0.0;
    let mut length = 0.0;
    for &id in consist {
        let stock = try_prototype(id)?;
        if !stock.length.is_finite()
            || stock.length <= 0.0
            || !stock.max_speed.is_finite()
            || stock.max_speed <= 0.0
            || !stock.acc_force.is_finite()
            || stock.acc_force < 0.0
            || !stock.dec_force.is_finite()
            || stock.dec_force <= 0.0
            || stock.price < Money::ZERO
        {
            return None;
        }
        seats += usize::from(stock.passenger_seats);
        mass = mass.checked_add(stock.mass)?;
        price = price.checked_add(stock.price.inner())?;
        upkeep = upkeep.checked_add(stock.passenger_upkeep_per_day.inner())?;
        acceleration += stock.acc_force;
        braking += stock.dec_force;
        length += stock.length;
    }
    (seats > 0
        && mass > 0
        && acceleration.is_finite()
        && acceleration > 0.0
        && braking.is_finite()
        && length.is_finite())
    .then_some((seats, Money::new_inner(price), Money::new_inner(upkeep)))
}

pub(super) struct RailLoop {
    pub times: Vec<u64>,
    paths: Vec<(Traversable, u64)>,
}

fn traversal_stamp(map: &Map, traversal: Traversable) -> Option<u64> {
    let points = traversal.raw_points(map)?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    traversal.hash(&mut hasher);
    for point in points.iter() {
        [point.x.to_bits(), point.y.to_bits(), point.z.to_bits()].hash(&mut hasher);
    }
    Some(hasher.finish())
}

pub(super) fn validate_loop(
    map: &Map,
    platforms: &BTreeMap<StopID, RailPlatformSpec>,
    calls: &[StopID],
    consist: &[RollingStockID],
) -> Result<RailLoop, Rejection> {
    if !(2..=30).contains(&calls.len())
        || calls.iter().copied().collect::<BTreeSet<_>>().len() != calls.len()
    {
        return Err(Rejection::InvalidConnection);
    }
    fleet_spec(consist).ok_or(Rejection::InvalidTrain)?;
    let length = train_length(consist);
    let stops = calls
        .iter()
        .map(|call| {
            let spec = platforms.get(call).ok_or(Rejection::MissingReference)?;
            let pose = platform_pose(map, *spec)?;
            if spec.length < length {
                return Err(Rejection::InvalidTrain);
            }
            Ok((spec, pose))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut paths = Vec::new();
    let times = stops.iter().zip(stops.iter().cycle().skip(1))
        .map(|((from, pose), (to, _))| {
            if from.lane == to.lane && from.offset >= to.offset {
                return Err(Rejection::InvalidConnection);
            }
            let route =
                Itinerary::route_rail(Tick(0), map, pose.berth, from.lane, to.lane, to.offset)
                    .ok_or(Rejection::InvalidConnection)?;
            if let Some(path) = route.get_route() {
                for &step in std::iter::once(&path.cur).chain(&path.reversed_route) {
                    if paths.len() == MAX_LOOP_TRAVERSALS {
                        return Err(Rejection::RoadWorkLimit);
                    }
                    let stamp = traversal_stamp(map, step).ok_or(Rejection::InvalidConnection)?;
                    paths.push((step, stamp));
                }
            }
            let metres = if from.lane == to.lane {
                to.offset - from.offset
            } else {
                let track = map.get(from.lane).ok_or(Rejection::MissingReference)?;
                let path = route.get_route().ok_or(Rejection::InvalidConnection)?;
                let mut distance = track.points.length() - from.offset + to.offset;
                for step in path.reversed_route.iter().skip(1) {
                    distance += step
                        .raw_points(map)
                        .ok_or(Rejection::InvalidConnection)?
                        .length();
                }
                distance
            };
            // Include acceleration and braking in the initial timetable estimate.
            Ok((metres / (16.0 * prototypes::DELTA)).ceil() as u64 + 80)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RailLoop { times, paths })
}

pub(super) fn new_line(
    schedule: RailSchedule,
    capacity: usize,
    consist: Vec<RollingStockID>,
    route: RailLoop,
    now: Tick,
) -> PassengerRailLine {
    let (_, _, daily_upkeep) = fleet_spec(&consist).unwrap();
    PassengerRailLine {
        schedule,
        pattern_revision: 0,
        capacity,
        missed_slots: 0,
        daily_upkeep,
        consist,
        leg_ticks: route.times,
        paths: route.paths,
        next_slot: schedule.slot_at_or_after(now.0.saturating_add(schedule.dwell_ticks)),
        removing: false,
    }
}

impl PassengerRailLine {
    pub(super) fn set_schedule(&mut self, schedule: RailSchedule, now: Tick) {
        self.schedule = schedule;
        self.next_slot = schedule.slot_at_or_after(now.0.saturating_add(schedule.dwell_ticks));
    }

    pub(super) fn install_loop(&mut self, route: RailLoop) {
        self.leg_ticks = route.times;
        self.paths = route.paths;
    }

    pub(super) fn remove(&mut self) {
        self.removing = true;
    }

    pub fn is_retiring(&self) -> bool {
        self.removing
    }
}

fn measured_berth(train: &TrainEnt, spec: RailPlatformSpec, berth: Vec3) -> bool {
    train.trans.pos.distance2(berth) <= BERTH_TOLERANCE.powi(2)
        && occupied_lanes(&train.res).any(|lane| lane == spec.lane)
        && (train.res.cur_travers_dist - spec.offset).abs() <= BERTH_TOLERANCE
}

fn at_platform(train: &TrainEnt, spec: RailPlatformSpec, berth: Vec3) -> bool {
    train.speed.0 <= STOPPED_SPEED && measured_berth(train, spec, berth)
}

pub(super) fn can_serve(
    train: &TrainEnt,
    run: &PassengerRailRun,
    rail: &RailTransit,
    line: &super::TransitLine,
    stop: &super::TransitStop,
) -> bool {
    run.close_at.is_some()
        && line.stops.get(run.next_call) == Some(&stop.id)
        && rail.platforms.get(&stop.id).is_some_and(|&spec| {
            rail.poses.get(&stop.id).is_some_and(|pose| {
                pose.sidewalk == stop.walk_pos && at_platform(train, spec, pose.berth)
            })
        })
}

pub fn rail_service_update(sim: &mut Simulation) {
    {
        let rail = sim.read::<RailTransit>();
        if rail.lines.is_empty() && rail.held.is_empty() {
            return;
        }
    }
    let now = elapsed_ticks(&sim.resources);
    let starts = {
        let (world, resources) = sim.world_res();
        let map = resources.read::<Map>();
        refresh_one_rail_line(world, resources, &map, now);
        let mut transit = resources.write::<Transit>();
        let mut rail = resources.write::<RailTransit>();
        let RailTransit {
            platforms,
            poses,
            lines,
            runs,
            riders,
            held,
            upkeep_pending,
            pending_lines,
            ..
        } = &mut *rail;
        let trains = resources.read::<ParCommandBuffer<TrainEnt>>();
        let mut counts = BTreeMap::<LineID, usize>::new();
        let mut starts = Vec::new();
        let mut suspend = BTreeSet::new();
        let mut ready = BTreeMap::<LineID, (TrainID, Tick)>::new();

        // Seen after shared boarding so the closing elapsed tick has its boarding pass.
        for (&id, run) in runs.iter_mut() {
            let (Some(train), Some(line), Some(config)) = (
                world.trains.get_mut(id),
                transit.lines.get(run.line),
                lines.get_mut(&run.line),
            ) else {
                continue;
            };
            let count = counts.entry(run.line).or_default();
            *count += 1;
            let retiring = config.removing || *count > usize::from(line.target_vehicles);
            let Some(stop) = line.stops.get(run.next_call) else {
                continue;
            };
            let platform = platforms[stop];
            if retiring && line.suspended && !riders.contains_key(&id) {
                trains.kill(id);
                continue;
            }
            if run.close_at.is_none() && !train.it.has_ended(0.0) {
                let previous = (run.next_call + line.stops.len() - 1) % line.stops.len();
                if now.0.saturating_sub(run.leg_started.0)
                    > config.leg_ticks[previous]
                        .saturating_mul(3)
                        .max(super::GIVE_UP_TICKS)
                {
                    suspend.insert(run.line);
                }
                continue;
            }
            let Some(pose) = poses.get(stop) else {
                suspend.insert(run.line);
                continue;
            };
            if run.close_at.is_none() {
                if !run.braking_at_berth {
                    let target = crate::map_dynamic::ItineraryTarget::DirectedLane(
                        platform.lane,
                        platform.offset,
                    );
                    if train.it.target() != Some(target)
                        || !measured_berth(train, platform, pose.berth)
                    {
                        suspend.insert(run.line);
                        continue;
                    }
                    train.it = Itinerary::NONE;
                    run.braking_at_berth = true;
                }
                if !at_platform(train, platform, pose.berth) {
                    continue;
                }
                run.braking_at_berth = false;
                let previous = (run.next_call + line.stops.len() - 1) % line.stops.len();
                let observed = now.0.saturating_sub(run.leg_started.0);
                let estimate = &mut config.leg_ticks[previous];
                *estimate = (7 * u128::from(*estimate) + u128::from(observed)).div_ceil(8) as u64;
                run.close_at = Some(Tick(now.0.saturating_add(config.schedule.dwell_ticks)));
                continue;
            }
            let close_at = run.close_at.unwrap();
            if !at_platform(train, platform, pose.berth) {
                continue;
            }
            if run.next_call == 0 && run.slot.is_none() && !retiring && !line.suspended {
                ready.entry(run.line).or_insert((id, close_at));
                continue;
            }
            if now < close_at {
                continue;
            }
            if retiring {
                // The service pass just gave every aboard rider this safe exit.
                trains.kill(id);
                continue;
            }
            if line.suspended {
                continue;
            }
            let next_call = (run.next_call + 1) % line.stops.len();
            let Some(target) = platforms
                .get(&line.stops[next_call])
                .filter(|_| poses.contains_key(&line.stops[next_call]))
            else {
                suspend.insert(run.line);
                continue;
            };
            let Some(route) = Itinerary::route_rail(
                now,
                &map,
                train.trans.pos,
                platform.lane,
                target.lane,
                target.offset,
            ) else {
                suspend.insert(run.line);
                continue;
            };
            train.it = route;
            run.next_call = next_call;
            run.call_sequence = run.call_sequence.saturating_add(1);
            run.close_at = None;
            run.leg_started = now;
            run.slot = None;
        }

        // One opening per line and no catch-up burst. An occupied first platform retains
        // the slot until its deadline; a ready train has priority over a purchase.
        for (&id, config) in lines.iter_mut() {
            let Some(line) = transit.lines.get(id) else {
                continue;
            };
            if config.removing || line.suspended || line.target_vehicles == 0 {
                continue;
            }
            let schedule = config.schedule;
            if now.0 > config.next_slot.saturating_add(schedule.max_late_ticks) {
                let next = schedule.slot_at_or_after(now.0);
                config.missed_slots = config
                    .missed_slots
                    .saturating_add((next - config.next_slot) / schedule.headway_ticks);
                config.next_slot = next;
            }
            let dwell_end = now.0.saturating_add(schedule.dwell_ticks);
            if dwell_end >= config.next_slot {
                if let Some(&(train, min_close)) = ready.get(&id) {
                    let Some(close) = schedule.departure_in_slot(
                        config.next_slot,
                        min_close.0.max(now.0.saturating_add(1)),
                    ) else {
                        continue;
                    };
                    let run = runs.get_mut(&train).unwrap();
                    run.slot = Some(config.next_slot);
                    run.close_at = Some(close);
                    config.next_slot = config.next_slot.saturating_add(schedule.headway_ticks);
                    continue;
                }
            }
            if dwell_end >= config.next_slot
                && schedule.departure_in_slot(config.next_slot, dwell_end).is_some()
                && counts.get(&id).copied().unwrap_or(0) < usize::from(line.target_vehicles)
            {
                let spec = platforms[&line.stops[0]];
                if poses.contains_key(&line.stops[0]) {
                    starts.push((id, spec, config.consist.clone()));
                }
            }
        }
        for line in suspend {
            transit.suspend(line, now);
        }
        for run in runs.values() {
            if let Some(config) = lines.get(&run.line) {
                *upkeep_pending += i128::from(config.daily_upkeep.inner());
            }
        }
        if now.0 % TICKS_PER_HOUR == 0 && *upkeep_pending != 0 {
            let charge = (*upkeep_pending / i128::from(TICKS_PER_BUDGET_DAY)) as i64;
            *upkeep_pending %= i128::from(TICKS_PER_BUDGET_DAY);
            Budget::apply(
                resources,
                BudgetCategory::TransitUpkeep,
                -Money::new_inner(charge),
            );
        }
        held.retain(|&id, hold| {
            world.edit_human(id, |human| {
                if now >= hold.deadline && human.router.target_dest.is_some() {
                    human.router.cancel_at_alight();
                }
                if now.0 % 256 == 0 {
                    let exit = platforms
                        .values()
                        .filter(|platform| Some(platform.lane) == hold.lane)
                        .filter_map(|&platform| platform_pose(&map, platform).ok())
                        .find(|pose| {
                            pose.berth.distance2(hold.position) <= BERTH_TOLERANCE.powi(2)
                        });
                    if let Some(pose) = exit {
                        Router::force_detach(human, pose.door, resources);
                    }
                }
            });
            world
                .humans
                .get(id)
                .is_some_and(|human| human.location == Location::TransitHeld)
        });
        let finished: Vec<_> = lines
            .iter()
            .filter(|(id, config)| config.removing && !counts.contains_key(id))
            .map(|(&id, _)| id)
            .collect();
        for id in finished {
            lines.remove(&id);
            pending_lines.remove(&id);
            transit.lines.remove(id);
            resources.write::<super::JourneyStats>().remove_line(id);
            transit.revision += 1;
        }
        starts
    };

    for (line, platform, consist) in starts {
        let Some((_, price, _)) = fleet_spec(&consist) else {
            continue;
        };
        if sim.read::<Government>().money < price {
            continue;
        }
        let Some(train) = spawn_train(
            sim,
            &consist,
            RailWagonKind::Passenger,
            platform.lane,
            platform.offset,
        ) else {
            continue;
        };
        let mut rail = sim.write::<RailTransit>();
        let config = rail.lines.get_mut(&line).unwrap();
        let slot = config.next_slot;
        config.next_slot = slot.saturating_add(config.schedule.headway_ticks);
        let close_at = Tick(slot.max(now.0.saturating_add(config.schedule.dwell_ticks)));
        rail.runs.insert(
            train,
            PassengerRailRun {
                line,
                next_call: 0,
                close_at: Some(close_at),
                braking_at_berth: false,
                leg_started: now,
                call_sequence: 0,
                slot: Some(slot),
            },
        );
        drop(rail);
        Budget::apply(&sim.resources, BudgetCategory::Construction, -price);
    }
}

pub(crate) fn rail_train_dropped(sim: &mut Simulation, id: TrainID, train: &TrainEnt) {
    let (world, resources) = sim.world_res();
    let mut rail = resources.write::<RailTransit>();
    let Some(run) = rail.runs.remove(&id) else {
        return;
    };
    let riders = rail.riders.remove(&id).unwrap_or_default();
    let map = resources.read::<Map>();
    let transit = resources.read::<Transit>();
    let exit = transit
        .lines
        .get(run.line)
        .and_then(|line| line.stops.get(run.next_call))
        .and_then(|stop| rail.platforms.get(stop).copied())
        .and_then(|spec| Some((spec, platform_pose(&map, spec).ok()?)))
        .filter(|(spec, pose)| at_platform(train, *spec, pose.berth))
        .map(|(_, pose)| pose);
    let now = elapsed_ticks(resources);
    for id_human in riders {
        world.edit_human(id_human, |human| {
            if let Some(pose) = exit {
                Router::force_detach(human, pose.boarding, resources);
                Router::walk_from_platform(human, pose.door, pose.sidewalk);
            } else {
                human.location = Location::TransitHeld;
                human.trans.pos = train.trans.pos;
                rail.held.insert(
                    id_human,
                    HeldRider {
                        position: train.trans.pos,
                        lane: crate::souls::freight_station::train_lane(&map, train),
                        deadline: Tick(now.0.saturating_add(super::GIVE_UP_TICKS)),
                    },
                );
            }
            resources.write::<super::JourneyStats>().discard_plan(id_human);
        });
    }
}

impl PassengerRailRun {
    pub(super) fn boarding_open(&self, now: Tick) -> bool {
        self.close_at.is_some_and(|close| now <= close)
            && (self.next_call != 0 || self.slot.is_some())
    }
}

pub(super) fn revalidate_rail(resources: &crate::utils::resources::Resources) {
    let mut rail = resources.write::<RailTransit>();
    if rail.platforms.is_empty() {
        return;
    }
    let map = resources.read::<Map>();
    let now = elapsed_ticks(resources);
    let mut transit = resources.write::<Transit>();
    let mut changed = BTreeSet::new();
    let mut poses = BTreeMap::new();
    let mut remapped = Vec::new();
    for (&id, &platform) in &rail.platforms {
        let binding = platform_pose(&map, platform).ok().map(|pose| (platform, pose))
            .or_else(|| {
                // Reconnect a retired track ID only to its same physical directed berth.
                let stop = transit.stops.get(id)?;
                let lane = map.nearest_lane(stop.berth, LaneKind::Rail, Some(BERTH_TOLERANCE))?;
                let track = map.get(lane)?;
                let offset = track.points.length_at_proj(track.points.project(stop.berth));
                let spec = RailPlatformSpec { lane, offset, ..platform };
                let pose = platform_pose(&map, spec).ok()?;
                (pose.berth.is_close(stop.berth, BERTH_TOLERANCE)
                    && pose.direction.xy().dot(stop.dir) >= 0.99)
                    .then_some((spec, pose))
            });
        if let Some((binding, pose)) = binding {
            if binding != platform {
                remapped.push((id, binding));
                changed.insert(id);
            }
            poses.insert(id, pose);
            let stop = &mut transit.stops[id];
            if (stop.berth, stop.walk_pos, stop.dir) != (pose.berth, pose.sidewalk, pose.direction.xy())
            {
                stop.pos = pose.berth;
                stop.berth = pose.berth;
                stop.walk_pos = pose.sidewalk;
                stop.dir = pose.direction.xy();
                changed.insert(id);
            }
        } else if rail.poses.contains_key(&id) {
            changed.insert(id);
        }
    }
    rail.platforms.extend(remapped);
    rail.poses = poses;
    let affected: Vec<_> = rail.lines.iter().filter_map(|(&id, line)| {
        let rebound = transit.lines[id].stops.iter().any(|stop| changed.contains(stop));
        let stale = line.paths.iter().any(|&(traversal, stamp)| {
            traversal_stamp(&map, traversal) != Some(stamp)
        });
        (rebound || stale).then_some(id)
    }).collect();
    for id in affected {
        rail.pending_lines.insert(id, ());
        transit.suspend(id, now);
    }
}

fn refresh_one_rail_line(
    world: &mut crate::World,
    resources: &crate::utils::resources::Resources,
    map: &Map,
    now: Tick,
) {
    let mut rail = resources.write::<RailTransit>();
    let Some((id, ())) = rail.pending_lines.pop_first() else {
        return;
    };
    let mut transit = resources.write::<Transit>();
    let (Some(line), Some(config)) = (transit.lines.get(id), rail.lines.get(&id)) else {
        return;
    };
    let Ok(route) = validate_loop(map, &rail.platforms, &line.stops, &config.consist) else {
        return;
    };
    let config = rail.lines.get_mut(&id).unwrap();
    config.pattern_revision += 1;
    config.install_loop(route);
    resources.write::<super::JourneyStats>().pattern_changed(id);
    transit.lines[id].suspended = false;
    transit.revision += 1;

    // A rail itinerary can be invalidated while the train remains on a retained lane.
    // Resume from measured live track when the directed path is available again.
    let RailTransit {
        runs, platforms, ..
    } = &mut *rail;
    for (&train_id, run) in runs {
        let line = &transit.lines[run.line];
        if run.line != id || line.suspended {
            continue;
        }
        let Some(train) = world.trains.get_mut(train_id) else {
            continue;
        };
        let stop = line.stops[run.next_call];
        let platform = platforms[&stop];
        let (lane, offset) = (platform.lane, platform.offset);
        if run.close_at.is_none() && !run.braking_at_berth && train.it.is_missing() {
            if let Some(from) = crate::souls::freight_station::train_lane(&map, train) {
                if let Some(route) =
                    Itinerary::route_rail(now, &map, train.trans.pos, from, lane, offset)
                {
                    train.it = route;
                    run.leg_started = now;
                }
            }
        }
    }
}

impl RailTransit {
    pub(crate) fn platform_walk_metres(&self, stop: StopID) -> f32 {
        self.poses.get(&stop).map_or(0.0, |pose| {
            pose.sidewalk.distance(pose.door) + pose.door.distance(pose.boarding)
        })
    }

    pub fn riders(&self, train: TrainID) -> impl Iterator<Item = HumanID> + '_ {
        self.riders
            .get(&train)
            .into_iter()
            .flat_map(|ids| ids.iter().copied())
    }

    pub(super) fn board(&mut self, train: TrainID, rider: HumanID) {
        self.riders.entry(train).or_default().insert(rider);
    }

    pub(super) fn alight(&mut self, train: TrainID, rider: HumanID) {
        if let Some(riders) = self.riders.get_mut(&train) {
            riders.remove(&rider);
            if riders.is_empty() {
                self.riders.remove(&train);
            }
        }
    }

    pub(crate) fn protects_station(&self, transit: &Transit, station: BuildingID) -> bool {
        self.runs.values().any(|run| {
            transit.lines.get(run.line).is_some_and(|line| {
                line.stops.iter().any(|stop| {
                    self.platforms
                        .get(stop)
                        .is_some_and(|platform| platform.station == station)
                })
            })
        })
    }

    /// Reject edits that can invalidate the installed path to the next safe exit.
    /// The edited junctions include both ends of any road the edit may reclip.
    pub(crate) fn check_occupied_egress(
        &self,
        transit: &Transit,
        world: &crate::World,
        map: &Map,
        affected: &[ProjectKind],
        budget: &mut PrepareBudget,
    ) -> Result<(), Rejection> {
        if self.riders.is_empty() {
            return Ok(());
        }
        let mut junctions = BTreeSet::new();
        for &project in affected {
            match project {
                ProjectKind::Road(id) => {
                    if let Some(road) = map.get(id) {
                        junctions.extend([road.src, road.dst]);
                    }
                }
                ProjectKind::Intersection(id) => {
                    junctions.insert(id);
                }
                _ => {}
            }
        }
        if junctions.is_empty() {
            return Ok(());
        }
        let affected_road = |id| {
            map.get(id).is_some_and(|road: &crate::map::Road| {
                junctions.contains(&road.src) || junctions.contains(&road.dst)
            })
        };
        let affected_lane = |id| map.get(id).is_some_and(|lane: &crate::map::Lane| affected_road(lane.parent));
        for &id in self.riders.keys() {
            budget.spend_road_work(1)?;
            let (Some(run), Some(train)) = (self.runs.get(&id), world.trains.get(id)) else {
                return Err(Rejection::OccupiedTrack);
            };
            let target = transit.lines.get(run.line)
                .and_then(|line| line.stops.get(run.next_call))
                .and_then(|stop| self.platforms.get(stop))
                .ok_or(Rejection::OccupiedTrack)?;
            if affected_lane(target.lane)
                || map.get(target.station)
                    .and_then(|station| station.connected_road)
                    .is_some_and(affected_road)
            {
                return Err(Rejection::OccupiedTrack);
            }
            for lane in occupied_lanes(&train.res) {
                budget.spend_road_work(1)?;
                if affected_lane(lane) {
                    return Err(Rejection::OccupiedTrack);
                }
            }
            if let Some(route) = train.it.get_route() {
                for traversal in std::iter::once(&route.cur).chain(&route.reversed_route) {
                    budget.spend_road_work(1)?;
                    let affected = match traversal.kind {
                        TraverseKind::Lane(lane) => affected_lane(lane),
                        TraverseKind::Turn(turn) => junctions.contains(&turn.parent),
                    };
                    if affected {
                        return Err(Rejection::OccupiedTrack);
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn forget_rail_rider(sim: &Simulation, id: HumanID, location: &Location) {
    match *location {
        Location::RailTransit(train) => sim.write::<RailTransit>().alight(train, id),
        Location::TransitHeld => {
            sim.write::<RailTransit>().held.remove(&id);
        }
        _ => {}
    }
}

pub(crate) fn validate_rail_snapshot(sim: &Simulation) -> Result<(), &'static str> {
    let mut rail = sim.write::<RailTransit>();
    let transit = sim.read::<Transit>();
    let map = sim.map();
    let now = elapsed_ticks(&sim.resources);
    if rail.pending_lines.keys().any(|&id| {
        !rail.lines.contains_key(&id) || transit.lines.get(id).is_none_or(|line| !line.suspended)
    }) {
        return Err("Invalid saved passenger rail refresh");
    }
    let mut poses = BTreeMap::new();
    for (&stop, platform) in &rail.platforms {
        let site = transit.stops.get(stop).ok_or("Missing saved passenger rail stop")?;
        if site.id != stop
            || !site.walk_pos.is_finite()
            || !site.berth.is_finite()
            || !platform.offset.is_finite()
            || !platform.length.is_finite()
            || platform.offset < platform.length
            || !(10.0..=250.0).contains(&platform.length)
        {
            return Err("Invalid saved passenger rail platform");
        }
        match platform_pose(&map, *platform) {
            Ok(pose) if (site.pos, site.berth, site.walk_pos, site.dir)
                == (pose.berth, pose.berth, pose.sidewalk, pose.direction.xy()) =>
            {
                poses.insert(stop, pose);
            }
            _ if rail.lines.keys().any(|&id| {
                transit.lines.get(id).is_some_and(|line| !line.suspended && line.stops.contains(&stop))
            }) => return Err("Invalid or unauthorized active passenger platform"),
            _ => {}
        }
    }
    for (&id, config) in &rail.lines {
        let Some(line) = transit.lines.get(id) else {
            return Err("Missing saved passenger rail line");
        };
        if line.id != id
            || !(2..=30).contains(&line.stops.len())
            || line.stops.iter().copied().collect::<BTreeSet<_>>().len() != line.stops.len()
            || line.target_vehicles > 10
            || (config.removing && line.target_vehicles != 0)
            || !(Money::ZERO..=Money::new_bucks(10)).contains(&line.fare)
            || !config.schedule.valid()
            || config.next_slot < config.schedule.anchor_elapsed.0
            || (config.next_slot - config.schedule.anchor_elapsed.0)
                % config.schedule.headway_ticks
                != 0
            || config.leg_ticks.len() != line.stops.len()
            || config.leg_ticks.iter().any(|&ticks| !(1..=10_000_000).contains(&ticks))
            || !line
                .stops
                .iter()
                .all(|stop| rail.platforms.contains_key(stop))
            || fleet_spec(&config.consist).is_none_or(|(seats, _, upkeep)| {
                seats != config.capacity || upkeep != config.daily_upkeep
            })
        {
            return Err("Invalid saved passenger rail line");
        }
    }
    let mut stock = BTreeMap::<TrainID, Vec<_>>::new();
    if !rail.runs.is_empty() {
        for wagon in sim.world().wagons.values() {
            let leader = wagon.itfollower.leader;
            if rail.runs.contains_key(&leader) {
                let wagons = stock.entry(leader).or_default();
                if wagons.len() == MAX_PASSENGER_STOCK {
                    return Err("Oversized saved passenger consist");
                }
                wagons.push((wagon.wagon.kind, wagon.wagon.rolling_stock));
            }
        }
    }
    let mut run_counts = BTreeMap::<LineID, usize>::new();
    for (&id, run) in &rail.runs {
        let (Some(train), Some(config), Some(line), Some(wagons)) = (
            sim.world().trains.get(id), rail.lines.get(&run.line), transit.lines.get(run.line), stock.get_mut(&id),
        ) else {
            return Err("Missing saved passenger rail run owner or consist");
        };
        let count = run_counts.entry(run.line).or_default();
        *count += 1;
        let locomotive = crate::transportation::train::calculate_locomotive(&config.consist);
        let mut expected = config.consist.clone();
        expected.sort_unstable();
        let mut actual: Vec<_> = wagons.iter().map(|&(_, stock)| stock).collect();
        actual.sort_unstable();
        if line.stops.get(run.next_call).is_none()
            || *count > 10
            || run.leg_started > now
            || run.braking_at_berth && run.close_at.is_some()
            || run.slot.is_some_and(|slot| run.next_call != 0 || run.close_at.is_none_or(|close| close.0 < slot))
            || run.close_at.is_some_and(|close| close.0 > now.0.saturating_add(6_600))
            || train.locomotive.length != locomotive.length
            || train.locomotive.max_speed != locomotive.max_speed
            || train.locomotive.acc_force != locomotive.acc_force
            || train.locomotive.dec_force != locomotive.dec_force
            || actual != expected
            || !wagons.iter().any(|&(kind, stock)| matches!(kind, RailWagonKind::Locomotive) && stock == config.consist[0])
            || wagons.iter().filter(|&&(kind, _)| matches!(kind, RailWagonKind::Locomotive)).count() != 1
            || wagons.iter().any(|&(kind, _)| matches!(kind, RailWagonKind::Freight))
        {
            return Err("Invalid saved passenger rail run");
        }
    }
    let mut riders = BTreeMap::<TrainID, BTreeSet<HumanID>>::new();
    let mut held_count = 0;
    for (id, human) in sim.world().humans.iter() {
        match human.location {
            Location::RailTransit(train) => {
                let Some(run) = rail.runs.get(&train) else {
                    return Err("Missing rider's passenger train");
                };
                let aboard = riders.entry(train).or_default();
                aboard.insert(id);
                let line = &transit.lines[run.line];
                if human.collider.is_some()
                    || aboard.len() > rail.lines[&run.line].capacity
                    || !poses.contains_key(&line.stops[run.next_call])
                    || !matches!(human.router.cur_step(),
                        Some(crate::map_dynamic::RoutingStep::RideTransit { line: riding, alight })
                        if *riding == run.line && line.stops.contains(alight))
                {
                    return Err("Invalid saved passenger train occupancy");
                }
            }
            Location::TransitHeld => {
                held_count += 1;
                if human.collider.is_some()
                    || rail.held.get(&id).is_none_or(|hold| {
                        !hold.position.is_finite() || hold.position != human.trans.pos
                    })
                {
                    return Err("Invalid saved passenger rail recovery");
                }
            }
            _ => {}
        }
    }
    if held_count != rail.held.len() {
        return Err("Orphaned saved passenger rail recovery");
    }
    rail.riders = riders;
    rail.poses = poses;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p8_w2_rejects_empty_saved_pattern() {
        let ctx = crate::tests::TestCtx::new_fast();
        let sim = &ctx.g;
        let line = super::super::insert_line(&mut sim.write::<Transit>(), Vec::new(), 1);
        let consist = vec![RollingStockID::new("passenger-emu-front")];
        let (capacity, _, _) = fleet_spec(&consist).unwrap();
        sim.write::<RailTransit>().lines.insert(
            line,
            new_line(RailSchedule::default(), capacity, consist, RailLoop { times: Vec::new(), paths: Vec::new() }, Tick(0)),
        );
        let bytes = sim
            .encode_slot(crate::SnapshotRevision::new(1))
            .unwrap()
            .into_snapshot_bytes();
        assert!(Simulation::load_compressed_snapshot(&bytes, 32 * 1024 * 1024).is_err());
    }
}

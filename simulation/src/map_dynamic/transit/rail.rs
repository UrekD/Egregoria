use std::collections::{BTreeMap, BTreeSet};

use geom::Vec3;
use prototypes::{try_prototype, Money, RollingStockID, Tick, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};

use crate::economy::{Budget, BudgetCategory, Government, TICKS_PER_BUDGET_DAY};
use crate::map::{BuildingID, BuildingKind, LaneID, LaneKind, Map};
use crate::map_dynamic::Itinerary;
use crate::transportation::train::{occupied_lanes, spawn_train, train_length, RailWagonKind};
use crate::transportation::Location;
use crate::world::{HumanID, TrainEnt, TrainID};
use crate::world_command::Rejection;
use crate::{ParCommandBuffer, Simulation};

use super::{elapsed_ticks, BusRun, LineID, Router, StopID, Transit};

pub(crate) const MAX_PASSENGER_STOCK: usize = 8;
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

    fn slot_at_or_after(self, tick: u64) -> u64 {
        let n = tick
            .saturating_sub(self.anchor_elapsed.0)
            .div_ceil(self.headway_ticks);
        self.anchor_elapsed
            .0
            .saturating_add(n.saturating_mul(self.headway_ticks))
    }

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
    pub(crate) consist: Vec<RollingStockID>,
    pub(crate) leg_ticks: Vec<u64>,
    next_slot: u64,
    removing: bool,
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
    slot: Option<u64>,
}

impl PassengerRailRun {
    pub fn next_platform_index(&self) -> usize {
        self.next_call
    }

    pub fn dwell_until(&self) -> Option<Tick> {
        self.close_at
    }

    pub(crate) fn forecast(&self, config: &PassengerRailLine) -> BusRun {
        BusRun {
            line: self.line,
            next_stop_idx: self.next_call,
            dwell_until: self.close_at.map(|close| {
                if self.next_call == 0 && self.slot.is_none() {
                    Tick(close.0.max(config.next_slot))
                } else {
                    close
                }
            }),
            leg_started: self.leg_started,
            failures: 0,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct RailTransit {
    pub platforms: BTreeMap<StopID, RailPlatformSpec>,
    pub lines: BTreeMap<LineID, PassengerRailLine>,
    pub runs: BTreeMap<TrainID, PassengerRailRun>,
    held: BTreeMap<HumanID, HeldRider>,
    #[serde(skip)]
    riders: BTreeMap<TrainID, BTreeSet<HumanID>>,
    upkeep_pending: i128,
}

#[derive(Serialize, Deserialize)]
struct HeldRider {
    position: Vec3,
    lane: Option<LaneID>,
    deadline: Tick,
}

pub(super) struct PlatformPose {
    pub door: Vec3,
    pub berth: Vec3,
    pub direction: Vec3,
}

pub(super) fn platform_pose(map: &Map, spec: RailPlatformSpec) -> Result<PlatformPose, Rejection> {
    let station = map.get(spec.station).ok_or(Rejection::MissingReference)?;
    let track = map.get(spec.lane).ok_or(Rejection::MissingReference)?;
    let sidewalk = station
        .connected_road
        .and_then(|road| map.get(road))
        .filter(|road| road.lanes_iter().any(|(_, kind)| kind == LaneKind::Walking));
    if station.kind != BuildingKind::TrainStation
        || sidewalk.is_none()
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
    let approach = track.points.point_along(spec.offset - spec.length);
    let door = station.door_pos;
    if door.distance(berth) > MAX_CONNECTOR_M
        || (door.z - berth.z).abs() > PLATFORM_HEIGHT_TOLERANCE
        || (door.z - approach.z).abs() > PLATFORM_HEIGHT_TOLERANCE
    {
        return Err(Rejection::InvalidConnection);
    }
    for (from, to) in [(door, berth), (berth, approach)] {
        let samples = (from.distance(to) / 2.0).ceil().max(1.0) as u32;
        for sample in 0..=samples {
            let point = from + (to - from) * (sample as f32 / samples as f32);
            if map
                .environment()
                .water_surface_elevation(point.xy())
                .is_some_and(|height| point.z <= height)
            {
                return Err(Rejection::InvalidConnection);
            }
        }
    }
    map.authorize_transit_site(door, door, berth, direction.xy(), track.parent)?;
    Ok(PlatformPose {
        door,
        berth,
        direction,
    })
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

pub(super) fn validate_loop(
    map: &Map,
    platforms: &BTreeMap<StopID, RailPlatformSpec>,
    calls: &[StopID],
    consist: &[RollingStockID],
) -> Result<Vec<u64>, Rejection> {
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
    stops
        .iter()
        .zip(stops.iter().cycle().skip(1))
        .map(|((from, pose), (to, _))| {
            if from.lane == to.lane && from.offset >= to.offset {
                return Err(Rejection::InvalidConnection);
            }
            let route =
                Itinerary::route_rail(Tick(0), map, pose.berth, from.lane, to.lane, to.offset)
                    .ok_or(Rejection::InvalidConnection)?;
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
        .collect()
}

pub(super) fn new_line(
    schedule: RailSchedule,
    capacity: usize,
    consist: Vec<RollingStockID>,
    leg_ticks: Vec<u64>,
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
        leg_ticks,
        next_slot: schedule.slot_at_or_after(now.0.saturating_add(schedule.dwell_ticks)),
        removing: false,
    }
}

impl PassengerRailLine {
    pub(super) fn set_schedule(&mut self, schedule: RailSchedule, now: Tick) {
        self.schedule = schedule;
        self.next_slot = schedule.slot_at_or_after(now.0.saturating_add(schedule.dwell_ticks));
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
    map: &Map,
    train: &TrainEnt,
    run: &PassengerRailRun,
    rail: &RailTransit,
    line: &super::TransitLine,
    stop: &super::TransitStop,
) -> bool {
    run.close_at.is_some()
        && line.stops.get(run.next_call) == Some(&stop.id)
        && rail.platforms.get(&stop.id).is_some_and(|&spec| {
            platform_pose(map, spec).is_ok_and(|pose| {
                pose.door == stop.walk_pos && at_platform(train, spec, pose.berth)
            })
        })
}

pub fn rail_service_update(sim: &mut Simulation) {
    if sim.read::<RailTransit>().lines.is_empty() && sim.read::<RailTransit>().held.is_empty() {
        return;
    }
    let now = elapsed_ticks(&sim.resources);
    let starts = {
        let (world, resources) = sim.world_res();
        let map = resources.read::<Map>();
        let mut transit = resources.write::<Transit>();
        let mut rail = resources.write::<RailTransit>();
        let RailTransit {
            platforms,
            lines,
            runs,
            held,
            upkeep_pending,
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
            let Some(&platform) = line
                .stops
                .get(run.next_call)
                .and_then(|stop| platforms.get(stop))
            else {
                continue;
            };
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
            let Ok(pose) = platform_pose(&map, platform) else {
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
                .filter(|&&spec| platform_pose(&map, spec).is_ok())
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
                if platform_pose(&map, spec).is_ok() {
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
    rail.riders.remove(&id);
    let map = resources.read::<Map>();
    let transit = resources.read::<Transit>();
    let exit = transit
        .lines
        .get(run.line)
        .and_then(|line| line.stops.get(run.next_call))
        .and_then(|stop| rail.platforms.get(stop).copied())
        .and_then(|spec| Some((spec, platform_pose(&map, spec).ok()?)))
        .filter(|(spec, pose)| at_platform(train, *spec, pose.berth))
        .map(|(_, pose)| pose.door);
    let now = elapsed_ticks(resources);
    world
        .humans
        .edit_active(crate::HumanWork::Transit, |id_human, human| {
            if human.location != Location::RailTransit(id) {
                return;
            }
            if let Some(pos) = exit {
                Router::force_detach(human, pos, resources);
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
        });
}

impl PassengerRailRun {
    pub(super) fn boarding_open(&self, now: Tick) -> bool {
        self.close_at.is_some_and(|close| now <= close)
            && (self.next_call != 0 || self.slot.is_some())
    }
}

pub(super) fn revalidate_rail(
    world: &mut crate::World,
    resources: &crate::utils::resources::Resources,
) {
    let mut rail = resources.write::<RailTransit>();
    if rail.platforms.is_empty() {
        return;
    }
    let map = resources.read::<Map>();
    let now = elapsed_ticks(resources);
    let mut transit = resources.write::<Transit>();
    let mut changed = BTreeSet::new();
    for (&id, &platform) in &rail.platforms {
        if let Ok(pose) = platform_pose(&map, platform) {
            let stop = &mut transit.stops[id];
            if (stop.berth, stop.walk_pos, stop.dir) != (pose.berth, pose.door, pose.direction.xy())
            {
                stop.pos = pose.berth;
                stop.berth = pose.berth;
                stop.walk_pos = pose.door;
                stop.dir = pose.direction.xy();
                changed.insert(id);
            }
        }
    }
    let ids: Vec<_> = rail.lines.keys().copied().collect();
    for id in ids {
        match validate_loop(
            &map,
            &rail.platforms,
            &transit.lines[id].stops,
            &rail.lines[&id].consist,
        ) {
            Ok(times) => {
                if transit.lines[id].suspended
                    || transit.lines[id]
                        .stops
                        .iter()
                        .any(|stop| changed.contains(stop))
                {
                    rail.lines.get_mut(&id).unwrap().pattern_revision += 1;
                    rail.lines.get_mut(&id).unwrap().leg_ticks = times;
                    transit.lines[id].suspended = false;
                    transit.revision += 1;
                }
            }
            Err(_) => transit.suspend(id, now),
        }
    }
    // A rail itinerary can be invalidated while the train remains on a retained lane.
    // Resume from measured live track when the directed path is available again.
    let RailTransit {
        runs, platforms, ..
    } = &mut *rail;
    for (&id, run) in runs {
        let line = &transit.lines[run.line];
        if line.suspended {
            continue;
        }
        let Some(train) = world.trains.get_mut(id) else {
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
    for (&stop, platform) in &rail.platforms {
        if !transit.stops.contains_key(stop)
            || !platform.offset.is_finite()
            || !platform.length.is_finite()
            || platform.offset < platform.length
            || !(10.0..=250.0).contains(&platform.length)
        {
            return Err("Invalid saved passenger rail platform");
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
    let mut riders = BTreeMap::<TrainID, BTreeSet<HumanID>>::new();
    for (&id, run) in &rail.runs {
        if !sim.world().trains.contains_key(id)
            || !rail.lines.contains_key(&run.line)
            || transit.lines[run.line].stops.get(run.next_call).is_none()
        {
            return Err("Invalid saved passenger rail run");
        }
    }
    for (id, human) in sim.world().humans.iter() {
        match human.location {
            Location::RailTransit(train) => {
                let Some(run) = rail.runs.get(&train) else {
                    return Err("Missing rider's passenger train");
                };
                let aboard = riders.entry(train).or_default();
                aboard.insert(id);
                if human.collider.is_some() || aboard.len() > rail.lines[&run.line].capacity {
                    return Err("Invalid saved passenger train occupancy");
                }
            }
            Location::TransitHeld => {
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
    rail.riders = riders;
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
            new_line(RailSchedule::default(), capacity, consist, Vec::new(), Tick(0)),
        );
        let bytes = sim
            .encode_slot(crate::SnapshotRevision::new(1))
            .unwrap()
            .into_snapshot_bytes();
        assert!(Simulation::load_compressed_snapshot(&bytes, 32 * 1024 * 1024).is_err());
    }
}

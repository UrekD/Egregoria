//! Transient directed traffic measurements for CITYMAP calibration.

use geom::{PolyLine3, Transform, Vec3};
use prototypes::{GameTime, Tick, TICKS_PER_MINUTE, TICKS_PER_REALTIME_SECOND};
use serde::Serialize;
use simulation::map::{
    LaneID, LaneKind, Map, PathKind, Pathfinder, TrafficBehavior, TraverseKind, TurnID,
};
use simulation::map_dynamic::{Itinerary, ItineraryTarget};
use simulation::transportation::{spawn_driving_vehicle, VehicleKind, VehicleState};
use simulation::{ParCommandBuffer, Simulation, VehicleEnt, VehicleID};
use std::collections::{BTreeMap, BTreeSet};

const SAMPLE_TICKS: u64 = TICKS_PER_MINUTE / 2;
const STUCK_TICKS: u64 = 20 * TICKS_PER_MINUTE;
const STOP_SPEED: f32 = 0.1;
const STOP_MOVEMENT: f32 = 0.5;
const STOP_LINE_GAP: f32 = 6.0;
const QUEUE_GAP: f32 = 3.0;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct LaneOffset {
    pub lane: LaneID,
    pub metres: f32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TripSpec {
    pub key: u32,
    pub departure_tick: u64,
    pub vehicle_kind: VehicleKind,
    pub origin_lane_offset: LaneOffset,
    pub destination_lane_offset: LaneOffset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum TripStatus {
    Planned,
    Waiting,
    Active,
    Arrived,
    Rejected,
    Lost,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TripRecord {
    pub spec: TripSpec,
    pub status: TripStatus,
    pub admitted_tick: Option<u64>,
    pub arrival_tick: Option<u64>,
    pub free_flow_ticks: Option<u64>,
    pub failure: Option<&'static str>,
}

struct Prepared {
    pose: Transform,
    destination: Vec3,
    itinerary: Itinerary,
    free_flow_ticks: u64,
}

impl Prepared {
    fn new(map: &Map, spec: TripSpec) -> Result<Self, &'static str> {
        let anchor = |offset: LaneOffset| {
            let lane = map
                .get(offset.lane)
                .filter(|l| l.kind == LaneKind::Driving)?;
            (offset.metres.is_finite() && (0.0..=lane.points.length()).contains(&offset.metres))
                .then(|| lane.points.point_dir_along(offset.metres))
        };
        let origin = spec.origin_lane_offset;
        let goal = spec.destination_lane_offset;
        let (pos, dir) = anchor(origin).ok_or("invalid origin anchor")?;
        let (destination, _) = anchor(goal).ok_or("invalid destination anchor")?;
        if origin.lane == goal.lane && goal.metres <= origin.metres {
            return Err("destination does not lie ahead on source lane");
        }
        if PathKind::Vehicle.nearest_lane(map, pos) != Some(origin.lane) {
            return Err("the router selected a different source lane");
        }
        let itinerary =
            Itinerary::route_to_lane(Tick(spec.departure_tick), pos, map, goal.lane, goal.metres)
                .ok_or("no directed route")?;
        let route = itinerary.get_route().ok_or("missing prepared route")?;
        if route.cur.kind != TraverseKind::Lane(origin.lane)
            || itinerary.target() != Some(ItineraryTarget::DirectedLane(goal.lane, goal.metres))
        {
            return Err("route does not match the directed anchors");
        }

        // Reference travel time for this release's actual routed path at the
        // speed limits, with the engine's unimpeded turn speed and type factor.
        let mut seconds = 0.0;
        for step in std::iter::once(&route.cur).chain(route.reversed_route.iter().rev()) {
            let points = step.raw_points(map).ok_or("missing traverse geometry")?;
            let mut metres = points.length();
            let speed = match step.kind {
                TraverseKind::Lane(lane) => {
                    if lane == origin.lane {
                        metres -= origin.metres;
                    }
                    if lane == goal.lane {
                        metres -= points.length() - goal.metres;
                    }
                    map.lanes()[lane].speed_limit
                }
                TraverseKind::Turn(_) => 9.0,
            };
            seconds += f64::from(metres / (speed * spec.vehicle_kind.speed_factor()));
        }
        Ok(Self {
            pose: Transform::new_dir(pos, dir),
            destination,
            itinerary,
            free_flow_ticks: (seconds * TICKS_PER_REALTIME_SECOND as f64).ceil() as u64,
        })
    }
}

struct Active {
    row: usize,
    destination: Vec3,
    traversal: Option<TraverseKind>,
    incoming_turn: Option<TurnID>,
    stationary: Option<(u64, Vec3)>,
    panicking: bool,
}

#[derive(Default, Serialize)]
pub struct Occupancy {
    pub speed_observations: u64,
    #[serde(skip)]
    speed_sum: f64,
    pub mean_speed_mps: Option<f64>,
    pub peak_active: usize,
    pub max_queue_m: f32,
    pub max_queue_vehicles: usize,
    pub max_queue_lane: Option<LaneID>,
    pub max_queue_tick: Option<u64>,
    pub standing_20m_episodes: u32,
    pub standing_20m_distinct_trips: usize,
    pub peak_standing_20m: usize,
    pub panics_started: u32,
    pub panics_recovered: u32,
    pub blocking_20m_episodes: u32,
    pub max_blocking_ticks: u64,
    pub max_blocking_lane: Option<LaneID>,
    pub max_blocking_turn: Option<TurnID>,
    pub last_blocking_tick: Option<u64>,
    pub last_queued_tick: Option<u64>,
    #[serde(skip)]
    pub directed_lane_entries: BTreeMap<LaneID, u32>,
    #[serde(skip)]
    pub directed_turn_entries: BTreeMap<TurnID, u32>,
}

#[derive(Clone, Copy)]
struct Standing {
    id: VehicleID,
    position: Vec3,
    offset: f32,
    radius: f32,
}

impl Standing {
    fn project(mut self, points: &PolyLine3) -> Option<Self> {
        let (projected, segment) = points.project_segment(self.position);
        let delta = self.position - projected;
        // Extend the endpoint tangent to keep the signed distance across the
        // turn/lane boundary. A clamped projection loses the rear footprint.
        let beyond = if projected == points.last() {
            delta.dot(points.last_dir()?).max(0.0)
        } else if projected == points.first() {
            delta.dot(points.first_dir()?).min(0.0)
        } else {
            0.0
        };
        if delta.mag2() - beyond * beyond > 1.0 {
            return None;
        }
        let prior = segment.saturating_sub(1);
        self.offset = points.as_slice()[..=prior]
            .windows(2)
            .map(|pair| pair[0].distance(pair[1]))
            .sum::<f32>()
            + points.as_slice()[prior].distance(projected)
            + beyond;
        Some(self)
    }

    fn on_lane(self, points: &PolyLine3) -> Option<Self> {
        self.project(points)
            .filter(|standing| (0.0..=points.length()).contains(&standing.offset))
    }

    fn overlapping_turn(self, map: &Map, turn: TurnID) -> Option<StandingTurn> {
        let points = &map
            .intersections()
            .get(turn.parent)?
            .find_turn(turn)?
            .points;
        let standing = self.project(points)?;
        let to_exit = points.length() - standing.offset;
        (standing.offset + standing.radius > 0.0 && to_exit + standing.radius > 0.0).then_some(
            StandingTurn {
                turn,
                to_exit,
                standing,
            },
        )
    }
}

struct StandingTurn {
    turn: TurnID,
    to_exit: f32,
    standing: Standing,
}

#[derive(Default)]
struct TrafficSample {
    lanes: BTreeMap<LaneID, Vec<Standing>>,
    turns: Vec<StandingTurn>,
    speed_sum: f64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(super) struct CutoffQueues {
    pub stop_line_vehicles: usize,
    pub midblock_vehicles: usize,
    pub conflict_vehicles: usize,
    pub waiting_entries: usize,
}

impl CutoffQueues {
    pub fn is_clear(self) -> bool {
        self.stop_line_vehicles == 0
            && self.midblock_vehicles == 0
            && self.conflict_vehicles == 0
            && self.waiting_entries == 0
    }
}

struct Blockage {
    since: u64,
    head: Standing,
    turn_vehicle: Standing,
    permissive_seen: bool,
    reported: bool,
}

impl Blockage {
    fn observe(
        &mut self,
        now: u64,
        head: Standing,
        turn_vehicle: Standing,
        permissive: bool,
    ) -> Option<(u64, bool)> {
        let stayed = |a: Standing, b: Standing| {
            a.id == b.id && a.position.distance2(b.position) <= STOP_MOVEMENT * STOP_MOVEMENT
        };
        if !stayed(self.head, head) || !stayed(self.turn_vehicle, turn_vehicle) {
            *self = Self {
                since: now,
                head,
                turn_vehicle,
                permissive_seen: false,
                reported: false,
            };
        }
        self.permissive_seen |= permissive;
        self.permissive_seen.then(|| {
            let duration = now - self.since;
            let crossed = duration >= STUCK_TICKS && !self.reported;
            self.reported |= crossed;
            (duration, crossed)
        })
    }
}

/// The fixture owns trip identities and removes its confirmed arrivals with the
/// normal simulation drop path. The occupancy toggle leaves launch and arrival
/// accounting active so instrumentation overhead can be measured on equal work.
pub struct TrafficCollector {
    records: Vec<TripRecord>,
    next: usize,
    waiting: BTreeMap<usize, Prepared>,
    active: BTreeMap<VehicleID, Active>,
    measure_occupancy: bool,
    occupancy: Occupancy,
    stood: BTreeSet<u32>,
    blockages: BTreeMap<(LaneID, TurnID), Blockage>,
}

impl TrafficCollector {
    pub fn new(mut specs: Vec<TripSpec>, measure_occupancy: bool) -> Result<Self, String> {
        let unique: BTreeSet<_> = specs.iter().map(|s| s.key).collect();
        if unique.len() != specs.len() {
            return Err("trip keys must be unique".into());
        }
        specs.sort_by_key(|s| (s.departure_tick, s.key));
        Ok(Self {
            records: specs
                .into_iter()
                .map(|spec| TripRecord {
                    spec,
                    status: TripStatus::Planned,
                    admitted_tick: None,
                    arrival_tick: None,
                    free_flow_ticks: None,
                    failure: None,
                })
                .collect(),
            next: 0,
            waiting: BTreeMap::new(),
            active: BTreeMap::new(),
            measure_occupancy,
            occupancy: Occupancy::default(),
            stood: BTreeSet::new(),
            blockages: BTreeMap::new(),
        })
    }

    /// Release at the current tick, then retry entrance blockage in release order.
    /// For a shared source lane the oldest waiting departure always goes first.
    pub fn release(&mut self, sim: &mut Simulation) {
        let now = sim.get_tick();
        while let Some(row) = self.records.get_mut(self.next) {
            if row.spec.departure_tick > now {
                break;
            }
            match Prepared::new(&sim.map(), row.spec) {
                Ok(prepared) => {
                    row.status = TripStatus::Waiting;
                    row.free_flow_ticks = Some(prepared.free_flow_ticks);
                    self.waiting.insert(self.next, prepared);
                }
                Err(reason) => {
                    row.status = TripStatus::Rejected;
                    row.failure = Some(reason);
                }
            }
            self.next += 1;
        }
        if self.waiting.is_empty() {
            return;
        }
        let mut blocked_lanes = BTreeSet::new();
        self.waiting.retain(|&index, prepared| {
            let row = &mut self.records[index];
            let lane = row.spec.origin_lane_offset.lane;
            if blocked_lanes.contains(&lane) {
                return true;
            }
            let Some(vehicle) = spawn_driving_vehicle(sim, row.spec.vehicle_kind, prepared.pose)
            else {
                blocked_lanes.insert(lane);
                return true;
            };
            let entity = &mut sim.world_mut_unchecked().vehicles[vehicle];
            entity.it = std::mem::replace(&mut prepared.itinerary, Itinerary::NONE);
            // Remove admission-order-dependent speed draws while retaining
            // the stock speed, acceleration and braking differences by type.
            entity.vehicle.max_speed_multiplier = 1.0;
            row.status = TripStatus::Active;
            row.admitted_tick = Some(now);
            self.active.insert(
                vehicle,
                Active {
                    row: index,
                    destination: prepared.destination,
                    traversal: None,
                    incoming_turn: None,
                    stationary: None,
                    panicking: false,
                },
            );
            false
        });
    }

    /// Observe after each simulation tick so transient traversals and arrivals
    /// are counted even when they fall between two occupancy samples.
    pub fn observe(&mut self, sim: &mut Simulation) {
        let now = sim.get_tick();
        let map = self.measure_occupancy.then(|| sim.map());
        let mut ended = false;
        let mut stationary_count = 0;
        self.active.retain(|&id, active| {
            let row = &mut self.records[active.row];
            let Some(vehicle) = sim.get(id) else {
                row.status = TripStatus::Lost;
                row.failure = Some("vehicle disappeared");
                return false;
            };
            let terminal = vehicle.it.has_ended(0.0);
            if terminal || vehicle.collider.is_none() || vehicle.vehicle.state.is_parked() {
                if terminal && vehicle.trans.pos.distance2(active.destination) <= 0.25 {
                    row.status = TripStatus::Arrived;
                    row.arrival_tick = Some(now);
                } else {
                    row.status = TripStatus::Lost;
                    row.failure =
                        Some("route ended away from directed target or lost its collider");
                }
                sim.read::<ParCommandBuffer<VehicleEnt>>().kill(id);
                ended = true;
                return false;
            }
            if let Some(map) = &map {
                stationary_count += usize::from(track_motion(
                    &mut self.occupancy,
                    map,
                    active,
                    vehicle,
                    now,
                    &mut self.stood,
                    row.spec.key,
                ));
            }
            true
        });
        drop(map);
        if ended {
            ParCommandBuffer::<VehicleEnt>::apply(sim);
        }
        if self.measure_occupancy {
            self.occupancy.peak_active = self.occupancy.peak_active.max(self.active.len());
            self.occupancy.peak_standing_20m =
                self.occupancy.peak_standing_20m.max(stationary_count);
            if now % SAMPLE_TICKS == 0 {
                self.sample(sim, now);
            }
        }
    }

    fn traffic_sample(&self, sim: &Simulation, map: &Map) -> TrafficSample {
        let mut sample = TrafficSample::default();
        for (&id, active) in &self.active {
            let vehicle = &sim.world().vehicles[id];
            sample.speed_sum += f64::from(vehicle.speed.0);
            if vehicle.speed.0.abs() > STOP_SPEED {
                continue;
            }
            let Some(traverse) = vehicle
                .it
                .get_travers()
                .map(|span| span.kind)
                .or(active.traversal)
            else {
                continue;
            };
            let standing = Standing {
                id,
                position: vehicle.trans.pos,
                offset: 0.0,
                radius: vehicle.vehicle.kind.width() * 0.5,
            };
            let (conflict, lanes) = match traverse {
                TraverseKind::Lane(lane) => (
                    active.incoming_turn.filter(|turn| turn.dst == lane),
                    [Some(lane), None],
                ),
                TraverseKind::Turn(turn) => (Some(turn), [Some(turn.dst), Some(turn.src)]),
            };
            if let Some(on_turn) = conflict.and_then(|turn| standing.overlapping_turn(map, turn)) {
                sample.turns.push(on_turn);
            } else if let Some((lane, on_lane)) = lanes
                .into_iter()
                .flatten()
                .find_map(|lane| Some((lane, standing.on_lane(&map.lanes().get(lane)?.points)?)))
            {
                sample.lanes.entry(lane).or_default().push(on_lane);
            }
        }
        for vehicles in sample.lanes.values_mut() {
            vehicles.sort_by(|a, b| b.offset.total_cmp(&a.offset).then(a.id.cmp(&b.id)));
        }
        sample
    }

    /// Snapshot the current physical state without altering the periodic metrics.
    pub(super) fn cutoff_queues(&self, sim: &Simulation) -> CutoffQueues {
        let map = sim.map();
        let sample = self.traffic_sample(sim, &map);
        let mut queues = CutoffQueues {
            conflict_vehicles: sample.turns.len(),
            waiting_entries: self.waiting.len(),
            ..CutoffQueues::default()
        };
        for (lane, vehicles) in sample.lanes {
            let stop_line = map.lanes()[lane].points.length();
            let stop_count = queue_tail(stop_line, vehicles.iter().map(|v| (v.offset, v.radius)))
                .map_or(0, |(_, _, count)| count);
            queues.stop_line_vehicles += stop_count;
            queues.midblock_vehicles += vehicles.len() - stop_count;
        }
        queues
    }

    fn sample(&mut self, sim: &Simulation, now: u64) {
        let map = sim.map();
        let TrafficSample {
            lanes,
            mut turns,
            speed_sum,
        } = self.traffic_sample(sim, &map);
        self.occupancy.speed_observations += self.active.len() as u64;
        self.occupancy.speed_sum += speed_sum;

        let mut queues = BTreeMap::new();
        for (lane, vehicles) in lanes {
            let stop_line = map.lanes()[lane].points.length();
            let Some((reach, tail, count)) =
                queue_tail(stop_line, vehicles.iter().map(|v| (v.offset, v.radius)))
            else {
                continue;
            };
            self.occupancy.last_queued_tick = Some(now);
            if reach > self.occupancy.max_queue_m {
                self.occupancy.max_queue_m = reach;
                self.occupancy.max_queue_lane = Some(lane);
                self.occupancy.max_queue_tick = Some(now);
            }
            self.occupancy.max_queue_vehicles = self.occupancy.max_queue_vehicles.max(count);
            queues.insert(lane, (vehicles[0], tail));
        }

        let mut observed = BTreeSet::new();
        let seconds = sim.read::<GameTime>().seconds;
        // Sort frontmost first if more than one car is standing on a turn.
        turns.sort_by(|a, b| {
            a.turn
                .cmp(&b.turn)
                .then(a.to_exit.total_cmp(&b.to_exit))
                .then(a.standing.id.cmp(&b.standing.id))
        });
        for StandingTurn {
            turn,
            to_exit,
            standing: turn_vehicle,
        } in turns
        {
            let Some(&(head, tail)) = queues.get(&turn.dst) else {
                continue;
            };
            if !blocks_entry(tail, to_exit - turn_vehicle.radius) {
                continue;
            }
            let key = (turn.dst, turn);
            if !observed.insert(key) {
                continue;
            }
            self.occupancy.last_blocking_tick = Some(now);
            let permissive = matches!(
                map.lanes()[turn.dst].control.get_behavior(seconds),
                TrafficBehavior::GREEN | TrafficBehavior::STOP
            );
            let episode = self.blockages.entry(key).or_insert(Blockage {
                since: now,
                head,
                turn_vehicle,
                permissive_seen: false,
                reported: false,
            });
            if let Some((duration, crossed)) = episode.observe(now, head, turn_vehicle, permissive)
            {
                self.occupancy.blocking_20m_episodes += u32::from(crossed);
                if duration > self.occupancy.max_blocking_ticks {
                    self.occupancy.max_blocking_ticks = duration;
                    self.occupancy.max_blocking_lane = Some(turn.dst);
                    self.occupancy.max_blocking_turn = Some(turn);
                }
            }
        }
        self.blockages.retain(|key, _| observed.contains(key));
    }

    pub fn finish(mut self) -> (Vec<TripRecord>, Occupancy) {
        self.occupancy.mean_speed_mps = (self.occupancy.speed_observations > 0)
            .then(|| self.occupancy.speed_sum / self.occupancy.speed_observations as f64);
        self.occupancy.standing_20m_distinct_trips = self.stood.len();
        (self.records, self.occupancy)
    }
}

fn track_motion(
    occupancy: &mut Occupancy,
    map: &Map,
    active: &mut Active,
    vehicle: &VehicleEnt,
    now: u64,
    stood: &mut BTreeSet<u32>,
    key: u32,
) -> bool {
    let traverse = vehicle.it.get_travers();
    let traversal = traverse.map(|t| t.kind);
    match traversal {
        Some(TraverseKind::Turn(turn)) => active.incoming_turn = Some(turn),
        Some(TraverseKind::Lane(lane))
            if active.incoming_turn.is_some_and(|turn| turn.dst != lane) =>
        {
            active.incoming_turn = None;
        }
        _ => {}
    }
    if active.traversal != traversal
        && traverse.is_none_or(|part| {
            part.raw_points(map)
                .is_some_and(|points| points.project_dist2(vehicle.trans.pos) <= 0.25)
        })
    {
        match traversal {
            Some(TraverseKind::Lane(lane)) => {
                *occupancy.directed_lane_entries.entry(lane).or_default() += 1
            }
            Some(TraverseKind::Turn(turn)) => {
                *occupancy.directed_turn_entries.entry(turn).or_default() += 1
            }
            None => {}
        }
        active.traversal = traversal;
    }
    let panicking = matches!(vehicle.vehicle.state, VehicleState::Panicking(_));
    occupancy.panics_started += u32::from(panicking && !active.panicking);
    occupancy.panics_recovered += u32::from(!panicking && active.panicking);
    active.panicking = panicking;
    if vehicle.speed.0.abs() > STOP_SPEED {
        active.stationary = None;
        return false;
    }
    let position = vehicle.trans.pos;
    let (since, origin) = active.stationary.get_or_insert((now, position));
    if position.distance2(*origin) > STOP_MOVEMENT * STOP_MOVEMENT {
        (*since, *origin) = (now, position);
        return false;
    }
    let duration = now - *since;
    if duration == STUCK_TICKS {
        occupancy.standing_20m_episodes += 1;
        stood.insert(key);
    }
    duration >= STUCK_TICKS
}

fn queue_tail(
    stop_line: f32,
    vehicles: impl IntoIterator<Item = (f32, f32)>,
) -> Option<(f32, f32, usize)> {
    let mut vehicles = vehicles.into_iter();
    let (front, radius) = vehicles.next()?;
    if stop_line - front - radius > STOP_LINE_GAP {
        return None;
    }
    let mut tail = front - radius;
    let mut count = 1;
    for (offset, radius) in vehicles {
        if tail - offset - radius > QUEUE_GAP {
            break;
        }
        tail = offset - radius;
        count += 1;
    }
    Some((stop_line - tail, tail, count))
}

fn blocks_entry(tail: f32, turn_front_to_entry: f32) -> bool {
    (tail + turn_front_to_entry).max(0.0) <= QUEUE_GAP
}

pub(super) fn scored_delay(
    release: u64,
    arrival: Option<u64>,
    cutoff: u64,
    free_flow: u64,
    limit: u64,
) -> u64 {
    let at_cutoff = arrival.filter(|&tick| tick <= cutoff);
    let delay = at_cutoff
        .unwrap_or(cutoff)
        .saturating_sub(release + free_flow);
    if at_cutoff.is_some() {
        delay
    } else {
        delay.max(limit + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cm0_queue_requires_a_contiguous_tail_and_an_occupied_incoming_turn() {
        let queued = [
            (25.0, 2.0),
            (20.0, 2.0),
            (15.0, 2.0),
            (10.0, 2.0),
            (5.0, 2.0),
        ];
        assert_eq!(queue_tail(28.0, queued), Some((25.0, 3.0, 5)));
        assert_eq!(
            queue_tail(28.0, [queued[0], queued[4]]),
            Some((5.0, 23.0, 1))
        );
        assert_eq!(queue_tail(40.0, queued), None);
        assert!(blocks_entry(2.0, 1.0));
        assert!(!blocks_entry(3.0, 12.0));
    }

    #[test]
    fn cm0_spillback_requires_no_progress_through_a_permissive_phase() {
        let head = Standing {
            id: VehicleID::default(),
            position: Vec3::ZERO,
            offset: 0.0,
            radius: 2.0,
        };
        let tail = Standing {
            position: Vec3::X,
            ..head
        };
        let mut episode = Blockage {
            since: 0,
            head,
            turn_vehicle: tail,
            permissive_seen: false,
            reported: false,
        };
        assert_eq!(
            episode.observe(7 * TICKS_PER_MINUTE, head, tail, false),
            None
        );
        assert_eq!(
            episode.observe(9 * TICKS_PER_MINUTE, head, tail, true),
            Some((900, false))
        );
        assert_eq!(
            episode.observe(STUCK_TICKS, head, tail, false),
            Some((STUCK_TICKS, true))
        );
        assert_eq!(
            episode.observe(STUCK_TICKS + SAMPLE_TICKS, head, tail, true),
            Some((STUCK_TICKS + SAMPLE_TICKS, false))
        );
        let moved = Standing {
            position: Vec3::Y,
            ..head
        };
        assert_eq!(
            episode.observe(STUCK_TICKS + 2 * SAMPLE_TICKS, moved, tail, true),
            Some((0, false))
        );
    }

    fn standing_vehicle(
        sim: &mut Simulation,
        collector: &mut TrafficCollector,
        lane: LaneID,
        offset: f32,
        goal: LaneOffset,
    ) -> VehicleID {
        let key = collector.records.len() as u32;
        let spec = TripSpec {
            key,
            departure_tick: sim.get_tick(),
            vehicle_kind: VehicleKind::Car,
            origin_lane_offset: LaneOffset {
                lane,
                metres: offset,
            },
            destination_lane_offset: goal,
        };
        let prepared = Prepared::new(&sim.map(), spec).unwrap();
        let id = spawn_driving_vehicle(sim, spec.vehicle_kind, prepared.pose).unwrap();
        sim.world_mut_unchecked().vehicles[id].it = prepared.itinerary;
        collector.records.push(TripRecord {
            spec,
            status: TripStatus::Active,
            admitted_tick: Some(sim.get_tick()),
            arrival_tick: None,
            free_flow_ticks: None,
            failure: None,
        });
        collector.active.insert(
            id,
            Active {
                row: key as usize,
                destination: prepared.destination,
                traversal: None,
                incoming_turn: None,
                stationary: None,
                panicking: false,
            },
        );
        id
    }

    #[test]
    fn cm0_blocked_exit_spans_turn_to_lane_handoff() {
        for centre_after_exit in [-0.75_f32, -0.25, 0.25, 3.0] {
            let (mut sim, source, goal) = super::super::spacing::cm0_boundary_fixture();
            let mut collector = TrafficCollector::new(Vec::new(), true).unwrap();
            let mut schedule = Simulation::schedule();
            let id = standing_vehicle(
                &mut sim,
                &mut collector,
                source,
                20.0,
                LaneOffset {
                    lane: goal,
                    metres: 38.0,
                },
            );
            let turn = (0..3000)
                .find_map(|_| {
                    sim.tick(&mut schedule, []);
                    collector.observe(&mut sim);
                    match sim.get(id)?.it.get_travers()?.kind {
                        TraverseKind::Turn(turn) => Some(turn),
                        _ => None,
                    }
                })
                .unwrap();
            if centre_after_exit > 0.0 {
                assert!((0..3000).any(|_| {
                    sim.tick(&mut schedule, []);
                    collector.observe(&mut sim);
                    sim.get(id).unwrap().it.get_travers().unwrap().kind
                        == TraverseKind::Lane(turn.dst)
                }));
            }
            let (position, length) = {
                let map = sim.map();
                let points = &map.intersections()[turn.parent]
                    .find_turn(turn)
                    .unwrap()
                    .points;
                let lane = &map.lanes()[turn.dst].points;
                let position = if centre_after_exit < 0.0 {
                    points.point_along(points.length() + centre_after_exit)
                } else {
                    lane.point_along(centre_after_exit)
                };
                (position, lane.length())
            };
            let gaps = ((length - 7.9) / 5.3).ceil() as usize;
            for index in 0..=gaps {
                let offset = 5.1 + (length - 7.9) * index as f32 / gaps as f32;
                standing_vehicle(
                    &mut sim,
                    &mut collector,
                    turn.dst,
                    offset,
                    LaneOffset {
                        lane: turn.dst,
                        metres: length,
                    },
                );
            }
            let vehicle = &mut sim.world_mut_unchecked().vehicles[id];
            vehicle.trans.pos = position;
            vehicle.speed.0 = 0.0;
            let start = sim.get_tick().next_multiple_of(SAMPLE_TICKS);
            for offset in (0..=STUCK_TICKS).step_by(SAMPLE_TICKS as usize) {
                collector.sample(&sim, start + offset);
            }
            let queues = collector.cutoff_queues(&sim);
            assert_eq!(
                queues.conflict_vehicles,
                usize::from(centre_after_exit < 2.25)
            );
            let (_, occupancy) = collector.finish();
            assert_eq!(
                occupancy.blocking_20m_episodes,
                u32::from(centre_after_exit < 2.25),
                "vehicle centre {centre_after_exit} m past exit"
            );
        }
    }

    #[test]
    fn cm0_cutoff_detects_both_queue_transitions_after_the_periodic_sample() {
        let (mut sim, lane, _) = super::super::spacing::cm0_boundary_fixture();
        let length = sim.map().lanes()[lane].points.length();
        let mut collector = TrafficCollector::new(Vec::new(), true).unwrap();
        let id = standing_vehicle(
            &mut sim,
            &mut collector,
            lane,
            length - 3.0,
            LaneOffset {
                lane,
                metres: length,
            },
        );
        let first_tick = sim.get_tick().next_multiple_of(SAMPLE_TICKS);
        let mut sample_tick = first_tick;
        let actual = [(0.0, 1.0), (1.0, 0.0)].map(|(sample_speed, cutoff_speed)| {
            sample_tick += SAMPLE_TICKS;
            sim.world_mut_unchecked().vehicles[id].speed.0 = sample_speed;
            collector.sample(&sim, sample_tick);
            sim.world_mut_unchecked().vehicles[id].speed.0 = cutoff_speed;
            sim.write::<GameTime>().tick = Tick(sample_tick + 11);
            let cutoff = collector.cutoff_queues(&sim);
            assert_eq!(cutoff.stop_line_vehicles, usize::from(cutoff_speed == 0.0));
            cutoff.is_clear()
        });
        assert_eq!(actual, [true, false]);
    }

    #[test]
    fn cm0_cutoff_counts_midblock_stops_and_entry_backlog() {
        let (mut sim, source, goal) = super::super::spacing::cm0_boundary_fixture();
        let spec = TripSpec {
            key: 0,
            departure_tick: sim.get_tick(),
            vehicle_kind: VehicleKind::Car,
            origin_lane_offset: LaneOffset {
                lane: source,
                metres: 20.0,
            },
            destination_lane_offset: LaneOffset {
                lane: goal,
                metres: 38.0,
            },
        };
        let mut collector =
            TrafficCollector::new(vec![spec, TripSpec { key: 1, ..spec }], true).unwrap();
        collector.release(&mut sim);
        let cutoff = collector.cutoff_queues(&sim);
        assert_eq!(cutoff.stop_line_vehicles, 0);
        assert_eq!(cutoff.midblock_vehicles, 1);
        assert_eq!(cutoff.conflict_vehicles, 0);
        assert_eq!(cutoff.waiting_entries, 1);
        assert!(!cutoff.is_clear());
    }

    #[test]
    fn cm0_penalizes_unfinished_trips_at_cutoff() {
        assert_eq!(scored_delay(100, Some(250), 300, 50, 100), 100);
        assert_eq!(scored_delay(100, None, 300, 50, 100), 150);
        assert_eq!(scored_delay(290, None, 300, 50, 100), 101);
    }
}

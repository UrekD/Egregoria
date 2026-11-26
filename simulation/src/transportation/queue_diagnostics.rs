use std::collections::{BTreeMap, BTreeSet};

use flat_spatial::grid::GridHandle;
use geom::{PolyLine3, Vec3};
use prototypes::{game_secs_to_ticks, GameTime};

use super::{ColliderOwnerLookup, TransportGrid, TraversalOwner, VehicleState};
use crate::map::{
    LaneID, LaneKind, Map, RoadID, TrafficBehavior, Traversable, TraverseDirection, TraverseKind,
    TurnID, TurnKind,
};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::{ParkingManagement, ParkingSearchStats, Transit};
use crate::utils::resources::Resources;
use crate::{Simulation, VehicleID, World};

const MAX_WATCHES: usize = 16;
const MAX_TRACES: usize = 256;
const SAMPLE_TICKS: u64 = game_secs_to_ticks(30);
const HOLD_DISTANCE: f32 = 6.0;
const QUEUE_GAP: f32 = 3.0;
const STANDING_SPEED: f32 = 0.2;
const PROGRESS: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueRevision(u64, u64, u64, u64);

impl QueueRevision {
    pub fn current(map: &Map) -> Self {
        Self(
            map.identity(),
            map.walking_topology_generation,
            map.driving_policy_generation,
            map.junction_policy_revision(),
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QueueLimits {
    pub occupants: usize,
    pub geometry_points: usize,
    pub hops: usize,
    pub alternatives: usize,
    pub parking_spots: usize,
    pub parking_owners: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            occupants: 128,
            geometry_points: 8_192,
            hops: 8,
            alternatives: 2,
            parking_spots: 128,
            parking_owners: 1_024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueHoldReason {
    Leader(GridHandle),
    Pedestrian(GridHandle),
    Conflict(Option<GridHandle>),
    ExitFull,
    Signal,
    StopDwell,
    BusDwell,
    InvalidTraversal,
    NoRoute,
    Unknown,
}

impl QueueHoldReason {
    fn collider(self) -> Option<GridHandle> {
        match self {
            Self::Leader(handle) | Self::Pedestrian(handle) => Some(handle),
            Self::Conflict(handle) => handle,
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueCoverage {
    Complete,
    Truncated,
    Unseen,
    StaleRevision,
    MissingLane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockerChainEnd {
    Clear,
    LawfulWait,
    Blocked,
    Cycle(TraversalOwner),
    Unseen,
    Truncated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockerHop {
    pub vehicle: VehicleID,
    pub reason: QueueHoldReason,
    pub blocker: Option<TraversalOwner>,
}

#[derive(Clone, Debug)]
pub struct QueueExplanation {
    pub lane: LaneID,
    pub revision: QueueRevision,
    pub elapsed_tick: u64,
    pub coverage: QueueCoverage,
    /// Contiguous stationary vehicles, front to rear, starting at the hold point.
    pub stop_line: Vec<VehicleID>,
    pub midblock: usize,
    pub omitted_occupants: usize,
    pub visited_occupants: usize,
    pub reason: Option<QueueHoldReason>,
    pub head_offset: Option<f32>,
    pub requested_turn: Option<TurnID>,
    pub hold_age_ticks: Option<u64>,
    pub omitted_holds: usize,
    pub chain: Vec<BlockerHop>,
    pub chain_end: BlockerChainEnd,
    /// Legal local movements with observed receiving room. No destination route is inferred.
    pub alternatives: Vec<TurnID>,
    pub omitted_alternatives: usize,
}

impl QueueExplanation {
    fn new(lane: LaneID, revision: QueueRevision, elapsed_tick: u64) -> Self {
        Self {
            lane,
            revision,
            elapsed_tick,
            coverage: QueueCoverage::Unseen,
            stop_line: Vec::new(),
            midblock: 0,
            omitted_occupants: 0,
            visited_occupants: 0,
            reason: None,
            head_offset: None,
            requested_turn: None,
            hold_age_ticks: None,
            omitted_holds: 0,
            chain: Vec::new(),
            chain_end: BlockerChainEnd::Unseen,
            alternatives: Vec::new(),
            omitted_alternatives: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkingAvailability {
    NoCurb,
    NoLocalAccess,
    Free,
    Full,
    Truncated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParkingPressure {
    pub road: RoadID,
    pub revision: QueueRevision,
    pub elevated: bool,
    pub total: usize,
    pub parked: usize,
    pub reserved: usize,
    pub reserved_inbound: usize,
    pub free: usize,
    /// Curb spaces with a driving and walking lane on the same directional side.
    pub accessible: usize,
    pub omitted_lanes: usize,
    pub omitted_spots: usize,
    pub visited_owners: usize,
    pub unclassified_reservations: usize,
    pub availability: ParkingAvailability,
    pub search: Option<ParkingSearchStats>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpillbackCounters {
    pub active_permissive_ticks: u64,
    pub longest_permissive_ticks: u64,
    pub episodes: u64,
    pub incomplete_ticks: u64,
}

#[derive(Default)]
struct SpillbackWatch {
    counts: SpillbackCounters,
    standing: Option<(VehicleID, f32)>,
    last_tick: Option<u64>,
    last_discharge: Option<u64>,
}

impl SpillbackWatch {
    fn update(&mut self, tick: u64, sample: Option<SpillbackSample>) {
        let consecutive = self
            .last_tick
            .is_some_and(|last| last.saturating_add(1) == tick);
        if self.last_tick == Some(tick) {
            return;
        }
        self.last_tick = Some(tick);
        let Some(sample) = sample else {
            self.counts.incomplete_ticks = self.counts.incomplete_ticks.saturating_add(1);
            self.standing = None;
            self.counts.active_permissive_ticks = 0;
            return;
        };
        let Some((id, offset)) = sample
            .standing
            .filter(|_| self.last_discharge != Some(tick))
        else {
            self.standing = None;
            self.counts.active_permissive_ticks = 0;
            return;
        };
        let continued = consecutive
            && self.standing.is_some_and(|(last_id, high_water)| {
                last_id == id && offset < high_water + PROGRESS
            });
        if !continued {
            self.standing = Some((id, offset));
            self.counts.active_permissive_ticks = 0;
        }
        if sample.permissive {
            if self.counts.active_permissive_ticks == 0 {
                self.counts.episodes = self.counts.episodes.saturating_add(1);
            }
            self.counts.active_permissive_ticks =
                self.counts.active_permissive_ticks.saturating_add(1);
            self.counts.longest_permissive_ticks = self
                .counts
                .longest_permissive_ticks
                .max(self.counts.active_permissive_ticks);
        }
    }
}

struct SpillbackSample {
    standing: Option<(VehicleID, f32)>,
    permissive: bool,
}

#[derive(Clone, Copy)]
struct HoldTrace {
    reason: QueueHoldReason,
    traversal: Traversable,
    since: u64,
    tick: u64,
}

/// Runtime-only observation. It is populated solely for explicitly watched movements.
#[derive(Default)]
pub struct QueueDiagnostics {
    revision: Option<QueueRevision>,
    watched_lanes: BTreeSet<LaneID>,
    watched_turns: BTreeMap<TurnID, SpillbackWatch>,
    holds: BTreeMap<VehicleID, HoldTrace>,
    omitted_holds: usize,
    presentation: BTreeMap<LaneID, QueueExplanation>,
    last_presentation_tick: Option<u64>,
    release_traversal_watch: bool,
}

impl QueueDiagnostics {
    pub fn presentation(&self, lane: LaneID) -> Option<&QueueExplanation> {
        self.presentation.get(&lane)
    }

    pub fn spillback(&self, turn: TurnID) -> Option<SpillbackCounters> {
        self.watched_turns.get(&turn).map(|watch| watch.counts)
    }

    fn unwatch_lane(&mut self, lane: LaneID) {
        self.watched_lanes.remove(&lane);
        self.presentation.remove(&lane);
    }

    fn unwatch_turn(&mut self, turn: TurnID) {
        self.watched_turns.remove(&turn);
    }

    pub(crate) fn record_discharge(&mut self, turn: TurnID, tick: u64) {
        if let Some(watch) = self.watched_turns.get_mut(&turn) {
            watch.last_discharge = Some(tick);
        }
    }

    pub(crate) fn observing(&self) -> bool {
        !self.watched_lanes.is_empty() || !self.watched_turns.is_empty()
    }

    pub(crate) fn begin_step(&mut self, map: &Map, tick: u64) {
        let revision = QueueRevision::current(map);
        if self.revision.replace(revision) != Some(revision) {
            self.holds.clear();
            self.presentation.clear();
            self.last_presentation_tick = None;
            for watch in self.watched_turns.values_mut() {
                *watch = SpillbackWatch::default();
            }
            self.watched_lanes.retain(|&lane| map.get(lane).is_some());
            self.watched_turns.retain(|turn, _| {
                map.get(turn.parent)
                    .and_then(|j| j.find_turn(*turn))
                    .is_some()
            });
            self.release_traversal_watch = !self.observing();
        }
        self.omitted_holds = 0;
        self.holds
            .retain(|_, trace| trace.tick.saturating_add(1) >= tick);
    }

    pub(crate) fn watches(&self, traversal: Option<Traversable>) -> bool {
        let Some(traversal) = traversal else {
            return false;
        };
        match traversal.kind {
            TraverseKind::Lane(lane) => {
                self.watched_lanes.contains(&lane)
                    || self
                        .watched_turns
                        .keys()
                        .any(|turn| [turn.src, turn.dst].contains(&lane))
            }
            TraverseKind::Turn(turn) => {
                self.watched_lanes.contains(&turn.src) || self.watched_turns.contains_key(&turn)
            }
        }
    }

    pub(crate) fn record_hold(
        &mut self,
        id: VehicleID,
        traversal: Option<Traversable>,
        tick: u64,
        reason: QueueHoldReason,
    ) {
        let Some(traversal) = traversal.filter(|traversal| self.watches(Some(*traversal))) else {
            return;
        };
        if self.holds.len() == MAX_TRACES && !self.holds.contains_key(&id) {
            self.omitted_holds = self.omitted_holds.saturating_add(1);
            return;
        }
        let since = self.holds.get(&id).map_or(tick, |last| {
            if last.reason == reason
                && last.traversal == traversal
                && last.tick.saturating_add(1) >= tick
            {
                last.since
            } else {
                tick
            }
        });
        self.holds.insert(
            id,
            HoldTrace {
                reason,
                traversal,
                since,
                tick,
            },
        );
    }
}

pub fn watch_queue(sim: &Simulation, lane: LaneID) -> bool {
    let map = sim.map();
    if !map
        .get(lane)
        .is_some_and(|lane| lane.kind == LaneKind::Driving)
    {
        return false;
    }
    let mut diagnostics = sim.write::<QueueDiagnostics>();
    if diagnostics.watched_lanes.len() >= MAX_WATCHES || !diagnostics.watched_lanes.insert(lane) {
        return false;
    }
    diagnostics.last_presentation_tick = None;
    diagnostics.begin_step(&map, elapsed_ticks(&sim.resources).0);
    drop(diagnostics);
    sim.write::<TransportGrid>().watch_queue_traversals(true);
    true
}

pub fn watch_spillback(sim: &Simulation, turn: TurnID) -> bool {
    let map = sim.map();
    if !map
        .get(turn.parent)
        .and_then(|junction| junction.find_turn(turn))
        .is_some_and(|turn| turn.kind == TurnKind::Driving)
    {
        return false;
    }
    let mut diagnostics = sim.write::<QueueDiagnostics>();
    if diagnostics.watched_turns.len() >= MAX_WATCHES
        || diagnostics.watched_turns.contains_key(&turn)
    {
        return false;
    }
    diagnostics
        .watched_turns
        .insert(turn, SpillbackWatch::default());
    diagnostics.begin_step(&map, elapsed_ticks(&sim.resources).0);
    drop(diagnostics);
    sim.write::<TransportGrid>().watch_queue_traversals(true);
    true
}

pub fn unwatch_queue(sim: &Simulation, lane: LaneID) {
    let mut diagnostics = sim.write::<QueueDiagnostics>();
    diagnostics.unwatch_lane(lane);
    let observing = diagnostics.observing();
    drop(diagnostics);
    sim.write::<TransportGrid>()
        .watch_queue_traversals(observing);
}

pub fn unwatch_spillback(sim: &Simulation, turn: TurnID) {
    let mut diagnostics = sim.write::<QueueDiagnostics>();
    diagnostics.unwatch_turn(turn);
    let observing = diagnostics.observing();
    drop(diagnostics);
    sim.write::<TransportGrid>()
        .watch_queue_traversals(observing);
}

/// Start collecting parking attempts whose nearest driving lane is on this road.
pub fn watch_parking(sim: &Simulation, road: RoadID) -> bool {
    let map = sim.map();
    map.get(road).is_some() && sim.write::<ParkingManagement>().watch_search(&map, road)
}

pub fn unwatch_parking(sim: &Simulation, road: RoadID) {
    sim.write::<ParkingManagement>().unwatch_search(road);
}

struct QueueReads<'a> {
    map: &'a Map,
    world: &'a World,
    grid: &'a TransportGrid,
    transit: &'a Transit,
    tick: u64,
    seconds: u32,
}

#[derive(Clone, Copy)]
struct Occupant {
    id: VehicleID,
    offset: f32,
    radius: f32,
    speed: f32,
    next: Option<TurnID>,
}

struct ReadBudget {
    occupants_left: usize,
    points_left: usize,
    visited: usize,
    omitted: usize,
}

impl ReadBudget {
    fn new(limits: QueueLimits) -> Self {
        Self {
            occupants_left: limits.occupants.min(256),
            points_left: limits.geometry_points.min(16_384),
            visited: 0,
            omitted: 0,
        }
    }
}

impl QueueReads<'_> {
    fn occupants(&self, traversal: Traversable, budget: &mut ReadBudget) -> Option<Vec<Occupant>> {
        let points = traversal.raw_points(self.map)?;
        let snapshot = self.grid.traversal_users(self.map, traversal)?;
        if snapshot.elapsed_tick != self.tick {
            return None;
        }
        let point_cost = points.len().max(1);
        let take = snapshot
            .len()
            .min(budget.occupants_left)
            .min(budget.points_left / point_cost);
        budget.omitted += snapshot.len() - take;
        budget.visited += take;
        budget.occupants_left -= take;
        budget.points_left -= take * point_cost;
        let occupants = snapshot
            .users()
            .take(take)
            .filter_map(|owner| {
                let TraversalOwner::Vehicle(id) = owner else {
                    return None;
                };
                let vehicle = self.world.vehicles.get(id)?;
                let collider = vehicle.collider?;
                if self.grid.get(collider.0).is_none()
                    || matches!(
                        vehicle.vehicle.state,
                        VehicleState::Parked(_) | VehicleState::RoadToPark(..)
                    )
                    || vehicle.it.get_travers() != Some(&traversal)
                {
                    return None;
                }
                let offset = directed_offset(points, vehicle.trans.pos, traversal.dir)?;
                let next = vehicle
                    .it
                    .get_route()
                    .and_then(|route| route.reversed_route.last())
                    .and_then(|next| match next.kind {
                        TraverseKind::Turn(turn) => Some(turn),
                        TraverseKind::Lane(_) => None,
                    });
                Some(Occupant {
                    id,
                    offset,
                    radius: vehicle.vehicle.kind.width() * 0.5,
                    speed: vehicle.speed.0.abs(),
                    next,
                })
            })
            .collect();
        Some(occupants)
    }

    fn has_receiving_room(
        &self,
        lane: LaneID,
        entering_length: f32,
        budget: &mut ReadBudget,
    ) -> Option<bool> {
        if self.map.get(lane)?.points.length() < entering_length + 1.5 {
            return Some(false);
        }
        let earlier = budget.omitted;
        let occupants = self.occupants(forward(TraverseKind::Lane(lane)), budget)?;
        if occupants
            .iter()
            .any(|occupant| occupant.offset - occupant.radius < entering_length + 1.5)
        {
            Some(false)
        } else {
            (budget.omitted == earlier).then_some(true)
        }
    }
}

fn directed_offset(points: &PolyLine3, pos: Vec3, direction: TraverseDirection) -> Option<f32> {
    let mut offset = 0.0;
    let mut best = (f32::INFINITY, 0.0);
    for segment in points.segments() {
        let projected = segment.project(pos);
        let distance2 = projected.distance2(pos);
        if distance2 < best.0 {
            best = (distance2, offset + segment.src.distance(projected));
        }
        offset += segment.src.distance(segment.dst);
    }
    // A cached traversal is a hint until the center has reached its deck.
    if best.0 > 2.5 * 2.5 {
        return None;
    }
    Some(match direction {
        TraverseDirection::Forward => best.1,
        TraverseDirection::Backward => offset - best.1,
    })
}

fn forward(kind: TraverseKind) -> Traversable {
    Traversable::new(kind, TraverseDirection::Forward)
}

pub fn explain_queue(
    sim: &Simulation,
    lane: LaneID,
    revision: QueueRevision,
    limits: QueueLimits,
) -> QueueExplanation {
    let map = sim.map();
    let grid = sim.read::<TransportGrid>();
    let transit = sim.read::<Transit>();
    let reads = QueueReads {
        map: &map,
        world: sim.world(),
        grid: &grid,
        transit: &transit,
        tick: elapsed_ticks(&sim.resources).0,
        seconds: sim.read::<GameTime>().seconds,
    };
    explain(
        &reads,
        &sim.read::<QueueDiagnostics>(),
        lane,
        revision,
        limits,
    )
}

fn explain(
    reads: &QueueReads,
    diagnostics: &QueueDiagnostics,
    lane: LaneID,
    revision: QueueRevision,
    limits: QueueLimits,
) -> QueueExplanation {
    let mut result = QueueExplanation::new(lane, revision, reads.tick);
    if revision != QueueRevision::current(reads.map) {
        result.coverage = QueueCoverage::StaleRevision;
        return result;
    }
    if diagnostics.revision == Some(revision) {
        result.omitted_holds = diagnostics.omitted_holds;
    }
    let Some(lane_data) = reads
        .map
        .get(lane)
        .filter(|lane| lane.kind == LaneKind::Driving)
    else {
        result.coverage = QueueCoverage::MissingLane;
        return result;
    };
    let mut budget = ReadBudget::new(limits);
    let Some(mut occupants) = reads.occupants(forward(TraverseKind::Lane(lane)), &mut budget)
    else {
        return result;
    };
    occupants.sort_unstable_by(|a, b| b.offset.total_cmp(&a.offset).then_with(|| a.id.cmp(&b.id)));
    if let Some(head) = occupants.first().filter(|head| {
        head.speed <= STANDING_SPEED
            && lane_data.points.length() - head.offset - head.radius <= HOLD_DISTANCE
    }) {
        let mut previous = *head;
        result.head_offset = Some(head.offset);
        result.requested_turn = head.next;
        result.stop_line.push(head.id);
        for &behind in &occupants[1..] {
            let gap = previous.offset - previous.radius - behind.offset - behind.radius;
            if behind.speed > STANDING_SPEED || gap > QUEUE_GAP {
                break;
            }
            result.stop_line.push(behind.id);
            previous = behind;
        }
        read_chain(reads, diagnostics, head.id, limits.hops.min(8), &mut result);
        if limits.alternatives != 0 {
            read_alternatives(
                reads,
                head,
                lane,
                limits.alternatives.min(2),
                &mut budget,
                &mut result,
            );
        }
    } else {
        result.chain_end = BlockerChainEnd::Clear;
    }
    result.midblock = occupants[result.stop_line.len()..]
        .iter()
        .filter(|vehicle| vehicle.speed <= STANDING_SPEED)
        .count();
    result.visited_occupants = budget.visited;
    result.omitted_occupants = budget.omitted;
    result.coverage = if budget.omitted > 0 || result.omitted_alternatives > 0 {
        QueueCoverage::Truncated
    } else {
        QueueCoverage::Complete
    };
    result
}

fn read_chain(
    reads: &QueueReads,
    diagnostics: &QueueDiagnostics,
    head: VehicleID,
    hops: usize,
    result: &mut QueueExplanation,
) {
    let mut current = head;
    let mut visited = BTreeSet::new();
    result.chain_end = BlockerChainEnd::Truncated;
    for _ in 0..hops {
        visited.insert(current);
        let trace = diagnostics.holds.get(&current).filter(|trace| {
            diagnostics.revision == Some(result.revision)
                && trace.tick == reads.tick
                && reads
                    .world
                    .vehicles
                    .get(current)
                    .and_then(|entity| entity.it.get_travers())
                    == Some(&trace.traversal)
        });
        let lawful_dwell = reads
            .transit
            .runs
            .get(&current)
            .and_then(|run| run.dwell_until)
            .is_some_and(|until| until.0 > reads.tick);
        let Some(reason) = lawful_dwell
            .then_some(QueueHoldReason::BusDwell)
            .or_else(|| trace.map(|trace| trace.reason))
        else {
            result.chain_end = if diagnostics.omitted_holds > 0 {
                BlockerChainEnd::Truncated
            } else {
                BlockerChainEnd::Unseen
            };
            return;
        };
        if current == head {
            result.reason = Some(reason);
            result.hold_age_ticks = trace.map(|trace| reads.tick.saturating_sub(trace.since));
        }
        let blocker = reason.collider().and_then(|handle| {
            match reads.grid.collider_owner(reads.world, handle) {
                ColliderOwnerLookup::Owned(owner) => Some(owner),
                ColliderOwnerLookup::Missing | ColliderOwnerLookup::Deferred => None,
            }
        });
        result.chain.push(BlockerHop {
            vehicle: current,
            reason,
            blocker,
        });
        match (reason, blocker) {
            (
                QueueHoldReason::Signal | QueueHoldReason::StopDwell | QueueHoldReason::BusDwell,
                _,
            ) => {
                result.chain_end = BlockerChainEnd::LawfulWait;
                return;
            }
            (_, Some(TraversalOwner::Vehicle(id))) if visited.contains(&id) => {
                result.chain_end = BlockerChainEnd::Cycle(TraversalOwner::Vehicle(id));
                return;
            }
            (_, Some(TraversalOwner::Vehicle(id))) => current = id,
            (_, None) if reason.collider().is_some() || reason == QueueHoldReason::Unknown => {
                result.chain_end = BlockerChainEnd::Unseen;
                return;
            }
            _ => {
                result.chain_end = BlockerChainEnd::Blocked;
                return;
            }
        }
    }
}

fn read_alternatives(
    reads: &QueueReads,
    head: &Occupant,
    lane: LaneID,
    limit: usize,
    budget: &mut ReadBudget,
    result: &mut QueueExplanation,
) {
    const MAX_VISITED_TURNS: usize = 64;
    let Some(junction) = reads.map.get(lane).and_then(|lane| reads.map.get(lane.dst)) else {
        return;
    };
    let turns = junction.turns();
    result.omitted_alternatives = turns.len().saturating_sub(MAX_VISITED_TURNS);
    for turn in turns.take(MAX_VISITED_TURNS).filter(|turn| {
        turn.kind == TurnKind::Driving
            && turn.id.src == lane
            && Some(turn.id) != head.next
            && reads.map.allows_turn(turn.id)
    }) {
        match reads.has_receiving_room(turn.id.dst, head.radius * 2.0, budget) {
            Some(true) if result.alternatives.len() < limit => result.alternatives.push(turn.id),
            Some(true) | None => result.omitted_alternatives += 1,
            Some(false) => {}
        }
    }
}

pub fn parking_pressure(
    sim: &Simulation,
    road: RoadID,
    limits: QueueLimits,
) -> Option<ParkingPressure> {
    let map = sim.map();
    let segment = map.get(road)?;
    let parking = sim.read::<ParkingManagement>();
    let mut result = ParkingPressure {
        road,
        revision: QueueRevision::current(&map),
        elevated: segment.elevated_span(),
        total: 0,
        parked: 0,
        reserved: 0,
        reserved_inbound: 0,
        free: 0,
        accessible: 0,
        omitted_lanes: 0,
        omitted_spots: 0,
        visited_owners: 0,
        unclassified_reservations: 0,
        availability: ParkingAvailability::NoCurb,
        search: parking.search_stats(&map, road),
    };
    let mut reserved = BTreeSet::new();
    let mut lanes_left = 32;
    let mut spots_left = limits.parking_spots.min(256);
    let mut free_accessible = 0;
    for direction in segment.directed_lanes() {
        let take = lanes_left.min(direction.len());
        lanes_left -= take;
        result.omitted_lanes += direction.len() - take;
        let direction = &direction[..take];
        let access = direction.iter().any(|&(_, kind)| kind == LaneKind::Driving)
            && direction.iter().any(|&(_, kind)| kind == LaneKind::Walking);
        for &(id, kind) in direction {
            if kind != LaneKind::Parking {
                continue;
            }
            let Some(spots) = map.parking.lane_spots.get(id) else {
                continue;
            };
            result.total += spots.len();
            if access {
                result.accessible += spots.len();
            }
            let take = spots_left.min(spots.len());
            spots_left -= take;
            result.omitted_spots += spots.len() - take;
            for &spot in &spots[..take] {
                if parking.is_spot_free(spot) {
                    result.free += 1;
                    free_accessible += usize::from(access);
                } else {
                    reserved.insert(spot);
                }
            }
        }
    }
    result.availability = if result.omitted_lanes != 0 {
        ParkingAvailability::Truncated
    } else if result.total == 0 {
        ParkingAvailability::NoCurb
    } else if result.accessible == 0 {
        ParkingAvailability::NoLocalAccess
    } else if free_accessible > 0 {
        ParkingAvailability::Free
    } else if result.omitted_spots != 0 {
        ParkingAvailability::Truncated
    } else {
        ParkingAvailability::Full
    };

    // Occupancy is attached to the vehicle's reservation. Count that claim once
    // even when the vehicle has physically reached the spot.
    result.reserved = reserved.len();
    if !reserved.is_empty() {
        for vehicle in sim
            .world()
            .vehicles
            .values()
            .take(limits.parking_owners.min(1_024))
        {
            result.visited_owners += 1;
            let (claim, parked) = match &vehicle.vehicle.state {
                VehicleState::Parked(claim) => (claim, true),
                VehicleState::RoadToPark(_, _, claim)
                | VehicleState::ReturningToDepot { spot: claim, .. } => (claim, false),
                _ => continue,
            };
            if reserved.remove(&claim.id()) {
                if parked {
                    result.parked += 1;
                } else {
                    result.reserved_inbound += 1;
                }
            }
            if reserved.is_empty() {
                break;
            }
        }
    }
    if result.visited_owners == sim.world().vehicles.len() {
        // Parking can be reserved while the claim is still held by a router.
        result.reserved_inbound += reserved.len();
    } else {
        result.unclassified_reservations = reserved.len();
    }
    Some(result)
}

fn spillback_sample(
    reads: &QueueReads,
    holds: &BTreeMap<VehicleID, HoldTrace>,
    turn: TurnID,
    budget: &mut ReadBudget,
) -> Option<SpillbackSample> {
    let lane = reads.map.get(turn.src)?;
    let omitted_before = budget.omitted;
    let occupants = reads.occupants(forward(TraverseKind::Turn(turn)), budget)?;
    if budget.omitted > omitted_before {
        return None;
    }
    let standing = occupants
        .iter()
        .filter(|occupant| occupant.speed <= STANDING_SPEED)
        .max_by(|a, b| a.offset.total_cmp(&b.offset).then_with(|| b.id.cmp(&a.id)));
    if let Some(standing) = standing {
        if !reads.has_receiving_room(turn.dst, standing.radius * 2.0, budget)? {
            return Some(SpillbackSample {
                standing: Some((standing.id, lane.points.length() + standing.offset)),
                permissive: matches!(
                    lane.control.get_behavior(reads.seconds),
                    TrafficBehavior::GREEN | TrafficBehavior::STOP
                ),
            });
        }
    }
    let approach = forward(TraverseKind::Lane(turn.src));
    let waiting = reads.occupants(approach, budget)?;
    if budget.omitted > omitted_before {
        return None;
    }
    let blocked = waiting
        .into_iter()
        .filter(|actor| {
            actor.next == Some(turn)
                && actor.speed <= STANDING_SPEED
                && lane.points.length() - actor.offset - actor.radius <= HOLD_DISTANCE
                && holds.get(&actor.id).is_some_and(|hold| {
                    hold.tick == reads.tick
                        && hold.traversal == approach
                        && hold.reason == QueueHoldReason::ExitFull
                })
        })
        .max_by(|a, b| a.offset.total_cmp(&b.offset).then_with(|| b.id.cmp(&a.id)));
    Some(SpillbackSample {
        standing: blocked.map(|actor| (actor.id, actor.offset)),
        // The actual barrier only publishes ExitFull after authored dwell.
        permissive: blocked.is_some(),
    })
}

pub fn queue_diagnostics_update(world: &mut World, resources: &mut Resources) {
    let mut diagnostics = resources.write::<QueueDiagnostics>();
    if !diagnostics.observing() {
        if std::mem::take(&mut diagnostics.release_traversal_watch) {
            resources
                .write::<TransportGrid>()
                .watch_queue_traversals(false);
        }
        return;
    }
    let map = resources.read::<Map>();
    let grid = resources.read::<TransportGrid>();
    let transit = resources.read::<Transit>();
    let tick = elapsed_ticks(resources).0;
    let reads = QueueReads {
        map: &map,
        world,
        grid: &grid,
        transit: &transit,
        tick,
        seconds: resources.read::<GameTime>().seconds,
    };
    let revision = QueueRevision::current(&map);
    let mut budget = ReadBudget::new(QueueLimits {
        occupants: 256,
        geometry_points: 16_384,
        ..QueueLimits::default()
    });
    let mut turns: Vec<_> = diagnostics.watched_turns.keys().copied().collect();
    if !turns.is_empty() {
        let start = (tick % turns.len() as u64) as usize;
        turns.rotate_left(start);
    }
    for turn in turns {
        let sample = spillback_sample(&reads, &diagnostics.holds, turn, &mut budget);
        diagnostics
            .watched_turns
            .get_mut(&turn)
            .unwrap()
            .update(tick, sample);
    }
    if diagnostics
        .last_presentation_tick
        .is_some_and(|last| tick.saturating_sub(last) < SAMPLE_TICKS)
    {
        return;
    }
    diagnostics.presentation = diagnostics
        .watched_lanes
        .iter()
        .map(|&lane| {
            (
                lane,
                explain(&reads, &diagnostics, lane, revision, QueueLimits::default()),
            )
        })
        .collect();
    diagnostics.last_presentation_tick = Some(tick);
}

#[cfg(test)]
#[path = "../tests/p7_queue_diagnostics.rs"]
mod tests;

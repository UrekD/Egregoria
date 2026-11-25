use crate::map::{
    ApproachRole, IntersectionID, LaneID, LightPolicy, Map, Traversable, TraverseDirection,
    TraverseKind, TurnID, TurnKind,
};
use crate::map_dynamic::Itinerary;
use crate::transportation::{
    ColliderOwnerLookup, Location, TransportGrid, TransportationGroup, Transporter, TraversalOwner,
};
use crate::world_command::bounded_command_vec;
use crate::{HumanID, VehicleID, World};
use geom::{Segment3, Vec2, Vec3};
use prototypes::{game_secs_to_ticks, DELTA};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const MAX_LOCAL_OWNERS: usize = 256;
const MAX_REQUESTS: usize = crate::map::MAX_CONFIGURED_JUNCTIONS * MAX_LOCAL_OWNERS;
const LOCAL_MARGIN: f32 = 36.0;
const MAX_PATH_POINTS: usize = 256;
const MAX_TURN_SEGMENT_PAIRS: usize = 16_384;
const STOP_DWELL: u64 = game_secs_to_ticks(1);
const AGE_PRIORITY: u64 = game_secs_to_ticks(60);
const GAP: f32 = 0.8;
const HOLD_TOLERANCE: f32 = 0.04;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum EntryOwner {
    Human(HumanID),
    Vehicle(VehicleID),
}

#[derive(Clone, Copy)]
pub(crate) struct MovingOwner {
    pub owner: EntryOwner,
    pub half_extent: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Crossing {
    turn: TurnID,
    backwards: bool,
}

impl Crossing {
    fn from_traversal(traversal: Traversable) -> Option<Self> {
        let TraverseKind::Turn(turn) = traversal.kind else {
            return None;
        };
        Some(Self {
            turn,
            backwards: traversal.dir == TraverseDirection::Backward,
        })
    }

    fn from_lane(self) -> LaneID {
        if self.backwards {
            self.turn.dst
        } else {
            self.turn.src
        }
    }

    fn to_lane(self) -> LaneID {
        if self.backwards {
            self.turn.src
        } else {
            self.turn.dst
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct WaitClock {
    crossing: Crossing,
    seen: u64,
    eligible_since: u64,
    stopped_since: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(try_from = "SavedAdmission", into = "SavedAdmission")]
#[derive(Clone)]
pub(crate) struct JunctionAdmission {
    waiting: BTreeMap<EntryOwner, WaitClock>,
    crossings: BTreeMap<EntryOwner, Crossing>,
}

#[derive(Serialize, Deserialize)]
struct SavedAdmission {
    #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_REQUESTS>")]
    waiting: Vec<(EntryOwner, WaitClock)>,
    #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_REQUESTS>")]
    crossings: Vec<(EntryOwner, Crossing)>,
}

impl From<JunctionAdmission> for SavedAdmission {
    fn from(state: JunctionAdmission) -> Self {
        Self {
            waiting: state.waiting.into_iter().collect(),
            crossings: state.crossings.into_iter().collect(),
        }
    }
}

impl TryFrom<SavedAdmission> for JunctionAdmission {
    type Error = &'static str;

    fn try_from(state: SavedAdmission) -> Result<Self, Self::Error> {
        if !state.waiting.windows(2).all(|pair| pair[0].0 < pair[1].0)
            || !state.crossings.windows(2).all(|pair| pair[0].0 < pair[1].0)
            || state.waiting.iter().any(|(_, clock)| {
                clock.eligible_since > clock.seen
                    || clock
                        .stopped_since
                        .is_some_and(|stopped| stopped > clock.seen)
            })
        {
            return Err("invalid junction admission state");
        }
        Ok(Self {
            waiting: state.waiting.into_iter().collect(),
            crossings: state.crossings.into_iter().collect(),
        })
    }
}

impl JunctionAdmission {
    pub(crate) fn clear(&mut self) {
        self.waiting.clear();
        self.crossings.clear();
    }

    pub(crate) fn crossing_turn(&self, turn: TurnID) -> bool {
        self.crossings
            .values()
            .any(|crossing| crossing.turn == turn)
    }

    pub(crate) fn reset_junction(&mut self, junction: IntersectionID) {
        self.waiting
            .retain(|_, clock| clock.crossing.turn.parent != junction);
    }

    pub(crate) fn crossing_junction(&self, junction: IntersectionID) -> bool {
        self.crossings
            .values()
            .any(|crossing| crossing.turn.parent == junction)
    }

    pub(crate) fn crossing_count(&self) -> usize {
        self.crossings.len()
    }

    pub(crate) fn validate_loaded(
        &mut self,
        map: &Map,
        world: &World,
        grid: &TransportGrid,
        tick: u64,
    ) -> Result<(), &'static str> {
        if self.waiting.values().any(|clock| clock.seen > tick) {
            return Err("junction admission clock is ahead of the simulation");
        }
        let valid_crossing = |crossing: Crossing| {
            (!crossing.backwards || crossing.turn.bidirectional)
                && map.allows_turn(crossing.turn)
                && configured(map, crossing.turn.parent).is_some()
        };
        let actor_on_grid = |owner| {
            let actor = Actor::get(world, owner)?;
            let expected = match owner {
                EntryOwner::Human(_) => TransportationGroup::Pedestrians,
                EntryOwner::Vehicle(_) => TransportationGroup::Vehicles,
            };
            (grid.get(actor.collider.0)?.1.group == expected).then_some(actor)
        };
        self.waiting.retain(|&owner, clock| {
            if self.crossings.contains_key(&owner)
                || !valid_crossing(clock.crossing)
                || tick.saturating_sub(clock.seen) > 1
            {
                return false;
            }
            let Some(actor) = actor_on_grid(owner) else {
                return false;
            };
            let Some(boundary) = actor.boundary(map) else {
                return false;
            };
            if !boundary.valid
                || !boundary.bounded
                || boundary.crossing != clock.crossing
                || !at_directed_approach(map, clock.crossing, &actor)
            {
                return false;
            }
            if boundary.distance > HOLD_TOLERANCE || actor.speed.abs() > 0.2 {
                clock.stopped_since = None;
            }
            true
        });
        self.crossings.retain(|&owner, crossing| {
            valid_crossing(*crossing)
                && actor_on_grid(owner).is_some_and(|actor| still_occupies(map, *crossing, &actor))
        });
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoldReason {
    Conflict,
    ExitFull,
    StopDwell,
    InvalidTraversal,
}

pub(crate) struct EntryBoundary {
    crossing: Crossing,
    pub distance: f32,
    pub valid: bool,
    pub controlled: bool,
    bounded: bool,
    overlaps: bool,
}

impl EntryBoundary {
    pub(crate) fn reached_by(&self, travel: f32) -> bool {
        travel > self.distance
    }
}

struct EntryRequest {
    user: MovingOwner,
    crossing: Crossing,
    role: ApproachRole,
    boundary: EntryBoundary,
    speed: f32,
}

#[derive(Clone, Copy)]
struct Sweep {
    owner: MovingOwner,
    crossing: Crossing,
}

struct Actor<'a> {
    owner: MovingOwner,
    collider: Transporter,
    itinerary: &'a Itinerary,
    position: Vec3,
    direction: Vec3,
    speed: f32,
}

impl<'a> Actor<'a> {
    fn get(world: &'a World, owner: EntryOwner) -> Option<Self> {
        let (itinerary, trans, speed, half_extent, collider) = match owner {
            EntryOwner::Vehicle(id) => {
                let vehicle = world.vehicles.get(id)?;
                let collider = vehicle.collider?;
                if vehicle.vehicle.state.is_parked() {
                    return None;
                }
                (
                    &vehicle.it,
                    &vehicle.trans,
                    vehicle.speed.0,
                    vehicle.vehicle.kind.width() * 0.5,
                    collider,
                )
            }
            EntryOwner::Human(id) => {
                let human = world.humans.get(id)?;
                let collider = human.collider?;
                if human.location != Location::Outside {
                    return None;
                }
                (&human.it, &human.trans, human.speed.0, 0.3, collider)
            }
        };
        Some(Self {
            owner: MovingOwner { owner, half_extent },
            collider,
            itinerary,
            position: trans.pos,
            direction: trans.dir,
            speed,
        })
    }

    fn boundary(&self, map: &Map) -> Option<EntryBoundary> {
        entry_boundary(
            map,
            self.itinerary,
            self.position,
            self.speed * DELTA,
            self.owner.half_extent,
        )
    }
}

#[derive(Default)]
pub(crate) struct JunctionFrame {
    grants: BTreeMap<EntryOwner, Crossing>,
    denied: BTreeMap<EntryOwner, HoldReason>,
}

pub(crate) fn configured(map: &Map, junction: IntersectionID) -> Option<LightPolicy> {
    let rules = map.junction_rules(junction)?;
    if rules.approaches().is_empty() {
        return None;
    }
    rules
        .effective_mode(map, junction)
        .filter(|&mode| mode != LightPolicy::Lights)
}

pub(crate) fn entry_boundary(
    map: &Map,
    it: &Itinerary,
    position: Vec3,
    travel: f32,
    half_extent: f32,
) -> Option<EntryBoundary> {
    if !map.has_junction_rules() {
        return None;
    }
    let route = it.get_route()?;
    let next = *route.reversed_route.last()?;
    let crossing = Crossing::from_traversal(next)?;
    let rules = map.junction_rules(crossing.turn.parent)?;
    let controlled = configured(map, crossing.turn.parent).is_some();
    if !controlled && rules.allows(crossing.turn) {
        return None;
    }
    // Endpoint proximity is a lower bound even for curved approach paths.
    let terminal = it.local_path().first().copied()?;
    if position.distance(terminal) > travel + half_extent + HOLD_TOLERANCE {
        return None;
    }
    let valid = map.allows_turn(crossing.turn)
        && matches!(route.cur.kind, TraverseKind::Lane(_))
        && route.cur.destination_lane() == crossing.from_lane()
        && route.cur.destination_intersection(map.lanes()) == Some(crossing.turn.parent)
        && (!crossing.backwards || crossing.turn.bidirectional);
    let distance = remaining_distance(it, position).map(|distance| distance - half_extent);
    Some(EntryBoundary {
        crossing,
        distance: distance.unwrap_or(0.0).max(0.0),
        valid,
        controlled,
        bounded: distance.is_some(),
        overlaps: distance.is_some_and(|distance| distance < -0.0001),
    })
}

fn remaining_distance(it: &Itinerary, position: Vec3) -> Option<f32> {
    let path = it.local_path();
    if path.len() > MAX_PATH_POINTS {
        return None;
    }
    Some(
        path.iter()
            .rev()
            .copied()
            .fold((position, 0.0), |(prev, length), next| {
                (next, length + prev.distance(next))
            })
            .1,
    )
}

impl JunctionFrame {
    pub(crate) fn prepare(
        world: &World,
        map: &Map,
        grid: &TransportGrid,
        state: &mut JunctionAdmission,
        tick: u64,
    ) -> Self {
        let mut frame = Self::default();
        let mut waiting = BTreeMap::new();
        let mut retained = BTreeMap::new();
        let mut deferred = BTreeSet::new();
        let mut occupied = BTreeMap::<IntersectionID, Vec<Sweep>>::new();
        for (&owner, &crossing) in &state.crossings {
            if let Some(actor) =
                Actor::get(world, owner).filter(|actor| still_occupies(map, crossing, actor))
            {
                occupied
                    .entry(crossing.turn.parent)
                    .or_default()
                    .push(Sweep {
                        owner: actor.owner,
                        crossing,
                    });
                retained.insert(owner, crossing);
            }
        }
        for junction in map.configured_junctions() {
            let Some(mode) = configured(map, junction) else {
                continue;
            };
            let inter = &map.intersections()[junction];
            let reach = Vec2::splat(inter.radius + LOCAL_MARGIN);
            let center = inter.pos.xy();
            let mut owners = BTreeSet::new();
            let candidates = grid.query(center - reach, center + reach);
            let mut complete = true;
            for (i, (handle, _)) in candidates.enumerate() {
                if i == MAX_LOCAL_OWNERS {
                    complete = false;
                    break;
                }
                match grid.collider_owner(world, handle) {
                    ColliderOwnerLookup::Owned(TraversalOwner::Vehicle(id)) => {
                        owners.insert(EntryOwner::Vehicle(id));
                    }
                    ColliderOwnerLookup::Owned(TraversalOwner::Human(id)) => {
                        owners.insert(EntryOwner::Human(id));
                    }
                    ColliderOwnerLookup::Deferred => complete = false,
                    _ => {}
                }
            }
            if !complete {
                deferred.insert(junction);
                continue;
            }
            let local = occupied.entry(junction).or_default();
            let actors: Vec<_> = owners
                .into_iter()
                .filter_map(|owner| Actor::get(world, owner))
                .filter(|actor| {
                    actor
                        .position
                        .xy()
                        .is_close(center, inter.radius + LOCAL_MARGIN)
                })
                .collect();
            let mut requests = Vec::new();
            for actor in &actors {
                let boundary = actor.boundary(map).filter(|boundary| {
                    boundary.crossing.turn.parent == junction && boundary.valid && boundary.bounded
                });
                let active = actor
                    .itinerary
                    .get_travers()
                    .copied()
                    .and_then(Crossing::from_traversal)
                    .filter(|crossing| {
                        crossing.turn.parent == junction && still_occupies(map, *crossing, actor)
                    })
                    .or_else(|| {
                        boundary
                            .as_ref()
                            .filter(|boundary| {
                                boundary.overlaps
                                    && at_directed_approach(map, boundary.crossing, actor)
                            })
                            .map(|b| b.crossing)
                    });
                if let Some(crossing) = active {
                    if retained.insert(actor.owner.owner, crossing) != Some(crossing) {
                        local.push(Sweep {
                            owner: actor.owner,
                            crossing,
                        });
                    }
                }
                let Some(boundary) =
                    boundary.filter(|b| retained.get(&actor.owner.owner) != Some(&b.crossing))
                else {
                    continue;
                };
                let role = match actor.owner.owner {
                    EntryOwner::Human(_) => ApproachRole::Yield,
                    EntryOwner::Vehicle(_) => {
                        let Some(lane) = map.get(boundary.crossing.from_lane()) else {
                            continue;
                        };
                        map.junction_rules(junction)
                            .and_then(|rules| rules.role(lane.parent))
                            .unwrap_or(if mode == LightPolicy::StopSigns {
                                ApproachRole::Stop
                            } else {
                                ApproachRole::Yield
                            })
                    }
                };
                requests.push(EntryRequest {
                    user: actor.owner,
                    crossing: boundary.crossing,
                    role,
                    boundary,
                    speed: actor.speed,
                });
            }
            frame.reserve(
                map,
                &state.waiting,
                &mut waiting,
                tick,
                local,
                &actors,
                requests,
            );
        }
        // Keep the arbitration age across a dense sample. A Stop claimant
        // must establish uninterrupted dwell again after the deferral.
        waiting.extend(state.waiting.iter().filter_map(|(&owner, &clock)| {
            deferred.contains(&clock.crossing.turn.parent).then_some((
                owner,
                WaitClock {
                    seen: tick,
                    stopped_since: None,
                    ..clock
                },
            ))
        }));
        state.waiting = waiting;
        state.crossings = retained;
        frame
    }

    fn reserve(
        &mut self,
        map: &Map,
        previous: &BTreeMap<EntryOwner, WaitClock>,
        waiting: &mut BTreeMap<EntryOwner, WaitClock>,
        tick: u64,
        occupied: &mut Vec<Sweep>,
        actors: &[Actor<'_>],
        requests: Vec<EntryRequest>,
    ) {
        let mut eligible = Vec::new();
        let mut heads = BTreeMap::new();
        for request in &requests {
            if matches!(request.user.owner, EntryOwner::Vehicle(_)) {
                let candidate = (request.boundary.distance, request.user.owner);
                let head = heads
                    .entry(request.crossing.from_lane())
                    .or_insert(candidate);
                if candidate < *head {
                    *head = candidate;
                }
            }
        }
        for request in requests {
            let owner = request.user.owner;
            let crossing = request.crossing;
            if matches!(owner, EntryOwner::Vehicle(_)) && heads[&crossing.from_lane()].1 != owner {
                self.denied.insert(owner, HoldReason::Conflict);
                continue;
            }
            let clock = previous.get(&owner).filter(|clock| {
                clock.crossing == crossing
                    && (clock.seen == tick || clock.seen.checked_add(1) == Some(tick))
            });
            let eligible_since = clock.map_or(tick, |clock| clock.eligible_since);
            let stopped_since = (request.boundary.distance <= HOLD_TOLERANCE
                && request.speed.abs() <= 0.2)
                .then(|| clock.and_then(|clock| clock.stopped_since).unwrap_or(tick));
            waiting.insert(
                owner,
                WaitClock {
                    crossing,
                    seen: tick,
                    eligible_since,
                    stopped_since,
                },
            );
            if request.role == ApproachRole::Stop
                && !stopped_since.is_some_and(|since| tick.saturating_sub(since) >= STOP_DWELL)
            {
                self.denied.insert(owner, HoldReason::StopDwell);
                continue;
            }
            let unaged = tick.saturating_sub(eligible_since) < AGE_PRIORITY;
            eligible.push((
                unaged,
                unaged.then_some(request.role),
                eligible_since,
                owner,
                request,
            ));
        }
        eligible
            .sort_unstable_by_key(|&(unaged, role, since, owner, _)| (unaged, role, since, owner));
        let mut work = MAX_TURN_SEGMENT_PAIRS;
        let mut aged_holds = Vec::new();
        for (unaged, _, _, owner, request) in eligible {
            let sweep = Sweep {
                owner: request.user,
                crossing: request.crossing,
            };
            if matches!(owner, EntryOwner::Vehicle(_)) {
                match receiving_room(map, &request, actors, &mut work) {
                    Some(true) => {}
                    Some(false) => {
                        self.denied.insert(owner, HoldReason::ExitFull);
                        continue;
                    }
                    None => {
                        self.denied.insert(owner, HoldReason::Conflict);
                        continue;
                    }
                }
            }
            if !request.boundary.reached_by(request.speed * DELTA) {
                continue;
            }
            if aged_holds
                .iter()
                .any(|&other| conflicts(map, sweep, other, &mut work))
            {
                self.denied.insert(owner, HoldReason::Conflict);
                continue;
            }
            if occupied
                .iter()
                .any(|&other| conflicts(map, sweep, other, &mut work))
            {
                self.denied.insert(owner, HoldReason::Conflict);
                if !unaged {
                    aged_holds.push(sweep);
                }
            } else {
                occupied.push(sweep);
                self.grants.insert(owner, sweep.crossing);
            }
        }
    }

    pub(crate) fn denial(&self, id: VehicleID) -> Option<HoldReason> {
        self.denied.get(&EntryOwner::Vehicle(id)).copied()
    }

    /// A grant is consumed only when the user would move their nose past the hold point.
    pub(crate) fn try_enter(
        &mut self,
        state: &mut JunctionAdmission,
        owner: EntryOwner,
        boundary: &EntryBoundary,
        on_initial_approach: bool,
    ) -> Result<(), HoldReason> {
        if !boundary.valid {
            return Err(HoldReason::InvalidTraversal);
        }
        if !boundary.controlled {
            return Ok(());
        }
        if !boundary.bounded {
            return Err(HoldReason::Conflict);
        }
        if on_initial_approach && state.crossings.get(&owner) == Some(&boundary.crossing) {
            return Ok(());
        }
        if self.grants.get(&owner) != Some(&boundary.crossing) {
            return Err(self
                .denied
                .get(&owner)
                .copied()
                .unwrap_or(HoldReason::Conflict));
        }
        if state.crossings.len() == MAX_REQUESTS && !state.crossings.contains_key(&owner) {
            return Err(HoldReason::Conflict);
        }
        self.grants.remove(&owner);
        state.waiting.remove(&owner);
        state.crossings.insert(owner, boundary.crossing);
        Ok(())
    }
}

fn receiving_room(
    map: &Map,
    request: &EntryRequest,
    actors: &[Actor<'_>],
    work: &mut usize,
) -> Option<bool> {
    let lane = map.get(request.crossing.to_lane())?;
    let needed = request.user.half_extent * 2.0 + GAP;
    if lane.points.length() < needed {
        return Some(false);
    }
    let start = lane.points.first();
    for actor in actors {
        *work = work.checked_sub(1)?;
        if actor.owner.owner == request.user.owner
            || !matches!(actor.owner.owner, EntryOwner::Vehicle(_))
            || !start
                .xy()
                .is_close(actor.position.xy(), needed + actor.owner.half_extent + 3.0)
        {
            continue;
        }
        // Model the footprint as an oriented capsule. Sweep only the storage
        // needed from the lane entrance and retain the deck separation.
        let body = actor.direction.xy().z0() * (actor.owner.half_extent - 1.0);
        let scale = |p: Vec3| p.xy().z(p.z * 2.0);
        let vehicle = Segment3::new(scale(actor.position - body), scale(actor.position + body));
        let mut remaining = needed;
        for segment in lane.points.segments() {
            *work = work.checked_sub(1)?;
            let length = segment.src.distance(segment.dst);
            if length == 0.0 {
                continue;
            }
            let tangent = segment.vec() / length;
            let storage_length = remaining.min(length);
            let center = (actor.position - segment.src).dot(tangent);
            let extent = body.dot(tangent).abs() + 1.0;
            // The entrance and its far edge are transverse cuts. Side
            // clearance must not inflate a bumper's longitudinal extent.
            if center + extent >= 0.0 && center - extent < storage_length {
                let end = segment.src + tangent * storage_length;
                let storage = Segment3::new(scale(segment.src), scale(end));
                if segments_close(vehicle, storage, (1.0 + lane.kind.width() * 0.5).powi(2)) {
                    return Some(false);
                }
            }
            remaining -= length;
            if remaining <= 0.0 {
                break;
            }
        }
    }
    Some(true)
}

fn at_directed_approach(map: &Map, crossing: Crossing, actor: &Actor<'_>) -> bool {
    let Some(lane) = map.get(crossing.from_lane()) else {
        return false;
    };
    if lane.points.len() > MAX_PATH_POINTS {
        return false;
    }
    let Some(current) = actor.itinerary.get_travers() else {
        return false;
    };
    let endpoint = match current.dir {
        TraverseDirection::Forward => lane.points.last(),
        TraverseDirection::Backward => lane.points.first(),
    };
    actor
        .itinerary
        .local_path()
        .first()
        .is_some_and(|p| p.is_close(endpoint, 0.05))
        && lane
            .points
            .project(actor.position)
            .is_close(actor.position, 0.8)
}

fn still_occupies(map: &Map, crossing: Crossing, actor: &Actor<'_>) -> bool {
    let Some(turn) = map
        .get(crossing.turn.parent)
        .and_then(|inter| inter.raw_turn(crossing.turn))
    else {
        return false;
    };
    let proper_mode = match actor.owner.owner {
        EntryOwner::Human(_) => matches!(turn.kind, TurnKind::Crosswalk | TurnKind::WalkingCorner),
        EntryOwner::Vehicle(_) => turn.kind == TurnKind::Driving,
    };
    if !proper_mode || turn.points.len() > MAX_PATH_POINTS {
        return false;
    }
    let Some(&current) = actor.itinerary.get_travers() else {
        return false;
    };
    let position = actor.position;
    let half_extent = actor.owner.half_extent;
    if let Some(active) = Crossing::from_traversal(current) {
        return crossing == active
            && turn.points.project(position).distance(position) <= half_extent + GAP;
    }
    if current.destination_lane() == crossing.from_lane() {
        let entry = if crossing.backwards {
            turn.points.last()
        } else {
            turn.points.first()
        };
        return entry.distance(position) < half_extent
            && at_directed_approach(map, crossing, actor)
            && actor.boundary(map).is_some_and(|boundary| {
                boundary.crossing == crossing && boundary.valid && boundary.overlaps
            });
    }
    if current.destination_lane() != crossing.to_lane()
        || current.destination_intersection(map.lanes()) == Some(crossing.turn.parent)
    {
        return false;
    }
    let exit = if crossing.backwards {
        turn.points.first()
    } else {
        turn.points.last()
    };
    exit.distance(position) <= half_extent + GAP
}

fn conflicts(map: &Map, first: Sweep, second: Sweep, work: &mut usize) -> bool {
    if first.owner.owner == second.owner.owner {
        return false;
    }
    if *work == 0 {
        return true;
    }
    *work -= 1;
    if first.crossing.from_lane() == second.crossing.from_lane()
        || first.crossing.to_lane() == second.crossing.to_lane()
    {
        return true;
    }
    let Some(intersection) = map.get(first.crossing.turn.parent) else {
        return true;
    };
    let Some((a, b)) = intersection
        .raw_turn(first.crossing.turn)
        .zip(intersection.raw_turn(second.crossing.turn))
    else {
        return true;
    };
    let clearance2 = (first.owner.half_extent + second.owner.half_extent + GAP).powi(2);
    for a in a.points.segments() {
        for b in b.points.segments() {
            if *work == 0 {
                return true;
            }
            *work -= 1;
            if segments_close(a, b, clearance2) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn segments_close(a: Segment3, b: Segment3, clearance2: f32) -> bool {
    if [a.src, a.dst]
        .iter()
        .any(|&p| b.project(p).distance2(p) <= clearance2)
        || [b.src, b.dst]
            .iter()
            .any(|&p| a.project(p).distance2(p) <= clearance2)
    {
        return true;
    }
    let a_dir = a.vec();
    let b_dir = b.vec();
    let normal = a_dir.cross(b_dir);
    let normal2 = normal.mag2();
    let offset = b.src - a.src;
    if normal2 == 0.0 || offset.dot(normal).powi(2) > clearance2 * normal2 {
        return false;
    }
    let range = 0.0..=normal2;
    range.contains(&offset.cross(b_dir).dot(normal))
        && range.contains(&offset.cross(a_dir).dot(normal))
}

#[cfg(test)]
#[test]
fn p7_junction_conflict_respects_crossing_decks_and_segment_interiors() {
    use geom::vec3;

    let route = Segment3::new(vec3(-10.0, 0.0, 0.0), vec3(10.0, 0.0, 0.0));
    let crossing = |deck| Segment3::new(vec3(0.0, -10.0, deck), vec3(0.0, 10.0, deck));
    assert!(segments_close(route, crossing(0.0), 0.25));
    assert!(!segments_close(route, crossing(6.0), 0.25));
    let disjoint = Segment3::new(vec3(-10.0, 4.0, 0.0), vec3(10.0, 4.0, 0.0));
    assert!(!segments_close(route, disjoint, 4.0));
}

#[cfg(test)]
impl JunctionAdmission {
    pub(crate) fn seed_saved_vehicle_claim(&mut self, vehicle: VehicleID, turn: TurnID, tick: u64) {
        let owner = EntryOwner::Vehicle(vehicle);
        let crossing = Crossing {
            turn,
            backwards: false,
        };
        self.crossings.insert(owner, crossing);
        self.waiting.insert(
            owner,
            WaitClock {
                crossing,
                seen: tick,
                eligible_since: 0,
                stopped_since: Some(0),
            },
        );
    }
}

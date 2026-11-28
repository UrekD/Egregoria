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
// Include sidewalk traffic in the discovery budget, separate from the
// entries and occupied movements that can participate in arbitration.
const MAX_ROUNDABOUT_CANDIDATES: usize = 512;
const MAX_REQUESTS: usize = crate::map::MAX_CONFIGURED_JUNCTIONS * MAX_LOCAL_OWNERS;
const LOCAL_MARGIN: f32 = 36.0;
const MAX_PATH_POINTS: usize = 256;
const MAX_TURN_SEGMENT_PAIRS: usize = 16_384;
const STOP_DWELL: u64 = game_secs_to_ticks(1);
const AGE_PRIORITY: u64 = game_secs_to_ticks(60);
pub(super) const GAP: f32 = 0.8;
pub(super) const VEHICLE_BODY_RADIUS: f32 = 1.0;
pub(crate) fn roundabout_exit_storage() -> f32 {
    super::VehicleKind::Bus.width() + GAP
}
const HOLD_TOLERANCE: f32 = 0.04;
const PEDESTRIAN_GAP_APPROACH: f32 = 3.0;
const PEDESTRIAN_GAP_AGE: u64 = game_secs_to_ticks(12);

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

    pub(crate) fn restore_migrated_crossing(
        &mut self,
        map: &Map,
        owner: EntryOwner,
        traversal: Option<&Traversable>,
    ) {
        // An actor on the outgoing lane can still occupy its saved crossing.
        // Snapshot validation checks that claim against the relocated actor.
        if self.crossings.len() >= MAX_REQUESTS && !self.crossings.contains_key(&owner) {
            return;
        }
        if let Some(crossing) = traversal.copied().and_then(Crossing::from_traversal) {
            if configured(map, crossing.turn.parent).is_some() && map.allows_turn(crossing.turn) {
                self.crossings.insert(owner, crossing);
            }
        }
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
                || map
                    .migrated_roundabout_paths
                    .contains_key(&TraverseKind::Turn(clock.crossing.turn))
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
struct Sweep<'a> {
    owner: MovingOwner,
    crossing: Crossing,
    remaining: Option<(Vec3, &'a [Vec3])>,
    footprint: Option<Segment3>,
    speed: f32,
    waiting_at_curb: bool,
}

impl<'a> Sweep<'a> {
    fn pending(owner: MovingOwner, crossing: Crossing, speed: f32) -> Self {
        Self {
            owner,
            crossing,
            speed,
            remaining: None,
            footprint: None,
            waiting_at_curb: false,
        }
    }

    fn occupied(map: &Map, crossing: Crossing, actor: &Actor<'a>) -> Self {
        let mut sweep = Self::pending(actor.owner, crossing, actor.speed);
        if !map.is_yield_roundabout(crossing.turn.parent) {
            return sweep;
        }
        if matches!(actor.owner.owner, EntryOwner::Vehicle(_)) {
            let body = actor.direction * (actor.owner.half_extent - VEHICLE_BODY_RADIUS);
            sweep.footprint = Some(Segment3::new(actor.position - body, actor.position + body));
            // A circulating vehicle protects the path ahead and its current footprint.
            sweep.remaining = match actor.itinerary.get_travers() {
                Some(step)
                    if Some(crossing) == Crossing::from_traversal(*step)
                        && actor.itinerary.local_path().len() <= MAX_PATH_POINTS =>
                {
                    Some((actor.position, actor.itinerary.local_path()))
                }
                Some(step) if step.kind == TraverseKind::Lane(crossing.to_lane()) => {
                    Some((actor.position, &[][..]))
                }
                _ => None,
            };
        } else {
            sweep.remaining = walking_suffix(map, crossing, actor);
        }
        sweep
    }
}

fn walking_suffix<'a>(
    map: &Map,
    crossing: Crossing,
    actor: &Actor<'a>,
) -> Option<(Vec3, &'a [Vec3])> {
    let current = *actor.itinerary.get_travers()?;
    if Crossing::from_traversal(current) != Some(crossing) {
        return (current.kind == TraverseKind::Lane(crossing.to_lane())
            || follows_crosswalk_onto_corner(map, crossing, current))
        .then_some((actor.position, &[][..]));
    }
    let turn = map.get(crossing.turn.parent)?.raw_turn(crossing.turn)?;
    let path = actor.itinerary.local_path();
    let [terminal] = path else {
        return None;
    };
    let exit = if crossing.backwards {
        turn.points.first()
    } else {
        turn.points.last()
    };
    // Merge only straight, directed routes whose remaining spans are nested.
    (turn.points.len() == 2
        && *terminal == exit
        && turn.points.project_dist2(actor.position) <= 0.01f32.powi(2))
    .then_some((actor.position, path))
}

#[derive(Default)]
struct Sweeps<'a> {
    vehicles: Vec<Sweep<'a>>,
    walkers: Vec<Sweep<'a>>,
    walking_turns: BTreeSet<(TurnID, bool)>,
}

impl<'a> Sweeps<'a> {
    fn insert(&mut self, sweep: Sweep<'a>, roundabout: bool) {
        if matches!(sweep.owner.owner, EntryOwner::Vehicle(_)) {
            self.vehicles.push(sweep);
        } else if !roundabout || sweep.remaining.is_some() {
            self.walkers.push(sweep);
        } else {
            if self.walking_turns.insert((
                sweep.crossing.turn,
                sweep.waiting_at_curb && sweep.crossing.backwards,
            )) {
                self.walkers.push(sweep);
            }
        }
    }

    fn conflict_owner(
        &self,
        map: &Map,
        sweep: Sweep<'_>,
        roundabout: bool,
        work: &mut usize,
    ) -> Option<EntryOwner> {
        let mut conflicts_with = |other: &&Sweep<'_>| {
            if roundabout {
                roundabout_conflicts(map, sweep, **other, work)
            } else {
                conflicts(map, sweep, **other, work)
            }
        };
        let consider_walkers = !roundabout || matches!(sweep.owner.owner, EntryOwner::Vehicle(_));
        self.vehicles
            .iter()
            .chain(self.walkers.iter().filter(|_| consider_walkers))
            .find(&mut conflicts_with)
            .map(|other| other.owner.owner)
    }
}

struct Recovery<'a> {
    owner: MovingOwner,
    position: Vec3,
    path: &'a [Vec3],
    speed: f32,
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
        let mut travel = self.speed * DELTA;
        if matches!(self.owner.owner, EntryOwner::Vehicle(_))
            && self
                .itinerary
                .get_route()
                .and_then(|route| route.reversed_route.last())
                .is_some_and(|next| {
                    matches!(next.kind, TraverseKind::Turn(turn)
                    if map.is_yield_roundabout(turn.parent))
                })
        {
            travel += PEDESTRIAN_GAP_APPROACH;
        }
        entry_boundary(
            map,
            self.itinerary,
            self.position,
            travel,
            self.owner.half_extent,
        )
    }
}

#[derive(Default)]
pub(crate) struct JunctionFrame {
    grants: BTreeMap<EntryOwner, Crossing>,
    denied: BTreeMap<EntryOwner, HoldReason>,
    recoveries: BTreeMap<EntryOwner, IntersectionID>,
}

pub(crate) fn configured(map: &Map, junction: IntersectionID) -> Option<LightPolicy> {
    if map.is_yield_roundabout(junction) {
        return Some(LightPolicy::NoLights);
    }
    let rules = map.junction_rules(junction)?;
    if rules.approaches().is_empty() {
        return None;
    }
    rules
        .effective_mode(map, junction)
        .filter(|&mode| mode != LightPolicy::Lights)
}

pub(super) fn controls_road_decision(map: &Map, it: &Itinerary, position: Vec3) -> bool {
    if !map.needs_junction_entry_checks() {
        return false;
    }
    match it.get_travers().map(|traversal| traversal.kind) {
        Some(TraverseKind::Turn(turn)) => configured(map, turn.parent).is_some(),
        Some(TraverseKind::Lane(lane)) => entry_boundary(map, it, position, LOCAL_MARGIN, 0.0)
            .is_some_and(|boundary| {
                boundary.controlled
                    && boundary.valid
                    && boundary.bounded
                    && boundary.distance <= LOCAL_MARGIN
                    // A short lane can lie entirely within the junction search margin.
                    && map.get(lane).is_some_and(|lane| boundary.distance <= lane.points.length() * 0.5)
            }),
        None => false,
    }
}

pub(crate) fn entry_boundary(
    map: &Map,
    it: &Itinerary,
    position: Vec3,
    travel: f32,
    half_extent: f32,
) -> Option<EntryBoundary> {
    if !map.needs_junction_entry_checks() {
        return None;
    }
    let route = it.get_route()?;
    let next = *route.reversed_route.last()?;
    let crossing = Crossing::from_traversal(next)?;
    let rules = map.junction_rules(crossing.turn.parent);
    let roundabout = map.is_yield_roundabout(crossing.turn.parent);
    let controlled = configured(map, crossing.turn.parent).is_some()
        && !(roundabout && turn_kind(map, crossing.turn) == Some(TurnKind::WalkingCorner));
    if !controlled && rules.is_none_or(|rules| rules.allows(crossing.turn)) {
        return None;
    }
    // Endpoint proximity is a lower bound even for curved approach paths.
    let terminal = it.local_path().first().copied()?;
    if position.distance(terminal) > travel + half_extent + HOLD_TOLERANCE {
        return None;
    }
    let valid = map.allows_turn(crossing.turn)
        && (route.cur.kind.is_lane() || follows_walking_turn(map, route.cur, crossing))
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

fn turn_kind(map: &Map, turn: TurnID) -> Option<TurnKind> {
    Some(map.get(turn.parent)?.raw_turn(turn)?.kind)
}

fn follows_walking_turn(map: &Map, current: Traversable, crossing: Crossing) -> bool {
    let Some(previous) = Crossing::from_traversal(current) else {
        return false;
    };
    let walking = |turn| {
        matches!(
            turn_kind(map, turn),
            Some(TurnKind::WalkingCorner | TurnKind::Crosswalk)
        )
    };
    previous.turn.parent == crossing.turn.parent
        && previous.to_lane() == crossing.from_lane()
        && walking(previous.turn)
        && walking(crossing.turn)
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
        outbound_fire: &impl Fn(VehicleID) -> bool,
    ) -> Self {
        let mut frame = Self::default();
        let mut waiting = BTreeMap::new();
        let mut retained = BTreeMap::new();
        let mut deferred = BTreeSet::new();
        let mut occupied = BTreeMap::<IntersectionID, Sweeps<'_>>::new();
        for (&owner, &crossing) in &state.crossings {
            if let Some(actor) =
                Actor::get(world, owner).filter(|actor| still_occupies(map, crossing, actor))
            {
                occupied.entry(crossing.turn.parent).or_default().insert(
                    Sweep::occupied(map, crossing, &actor),
                    map.is_yield_roundabout(crossing.turn.parent),
                );
                retained.insert(owner, crossing);
            }
        }
        for junction in map.configured_junctions() {
            let Some(mode) = configured(map, junction) else {
                continue;
            };
            let inter = &map.intersections()[junction];
            let roundabout = map.is_yield_roundabout(junction);
            let reach = Vec2::splat(inter.radius + LOCAL_MARGIN);
            let center = inter.pos.xy();
            let mut owners = BTreeSet::new();
            let candidate_limit = if roundabout {
                MAX_ROUNDABOUT_CANDIDATES
            } else {
                MAX_LOCAL_OWNERS
            };
            let mut complete = true;
            for (i, (handle, position)) in grid.query(center - reach, center + reach).enumerate() {
                if i == candidate_limit {
                    complete = false;
                    break;
                }
                if roundabout && !position.is_close(center, reach.x) {
                    continue;
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
            let vehicles = actors
                .iter()
                .filter(|actor| matches!(actor.owner.owner, EntryOwner::Vehicle(_)))
                .collect::<Vec<_>>();
            let mut active_owners = vehicles.len();
            let mut requests = Vec::new();
            let mut recoveries = Vec::new();
            for actor in &actors {
                if actor.itinerary.egress_junction(map) == Some(junction) {
                    if let Some(path) = actor.itinerary.recovery_path(map, actor.position) {
                        active_owners += 1;
                        recoveries.push(Recovery {
                            owner: actor.owner,
                            position: actor.position,
                            path,
                            speed: actor.speed,
                        });
                    }
                    continue;
                }
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
                                    // Roundabout pedestrians at either sidewalk mouth
                                    // can safely wait for a grant, even on the last point.
                                    && (!roundabout || matches!(actor.owner.owner, EntryOwner::Vehicle(_)))
                                    && at_directed_approach(map, boundary.crossing, actor)
                            })
                            .map(|b| b.crossing)
                    });
                if matches!(actor.owner.owner, EntryOwner::Human(_))
                    && (active.is_some() || boundary.is_some())
                {
                    active_owners += 1;
                }
                if let Some(crossing) = active {
                    let owner = actor.owner.owner;
                    if retained.len() >= MAX_REQUESTS && !retained.contains_key(&owner) {
                        complete = false;
                        break;
                    }
                    if retained.insert(owner, crossing) != Some(crossing) {
                        local.insert(Sweep::occupied(map, crossing, actor), roundabout);
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
            if !complete || active_owners > MAX_LOCAL_OWNERS {
                deferred.insert(junction);
                continue;
            }
            frame.reserve(
                map,
                &state.waiting,
                &mut waiting,
                tick,
                local,
                &vehicles,
                &recoveries,
                requests,
                junction,
                outbound_fire,
            );
        }
        // Keep the arbitration age across a dense sample. A Stop claimant
        // must establish uninterrupted dwell again after the deferral.
        for (&owner, &clock) in &state.waiting {
            if waiting.len() >= MAX_REQUESTS {
                break;
            }
            if deferred.contains(&clock.crossing.turn.parent) {
                waiting.entry(owner).or_insert(WaitClock {
                    seen: tick,
                    stopped_since: None,
                    ..clock
                });
            }
        }
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
        occupied: &mut Sweeps<'_>,
        vehicles: &[&Actor<'_>],
        recoveries: &[Recovery<'_>],
        requests: Vec<EntryRequest>,
        junction: IntersectionID,
        outbound_fire: &impl Fn(VehicleID) -> bool,
    ) {
        let roundabout = map.is_yield_roundabout(junction);
        let mut work = MAX_TURN_SEGMENT_PAIRS;
        for recovery in recoveries {
            if recovery_can_move(map, junction, recovery, vehicles, &mut work) {
                self.recoveries.insert(recovery.owner.owner, junction);
            }
        }
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
            if waiting.len() >= MAX_REQUESTS && !waiting.contains_key(&owner) {
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
                !unaged || !matches!(owner, EntryOwner::Vehicle(id) if outbound_fire(id)),
                eligible_since,
                owner,
                request,
            ));
        }
        eligible
            .sort_unstable_by_key(|&(unaged, role, emergency, since, owner, _)| {
                (unaged, role, emergency, since, owner)
            });
        let mut aged_holds = Sweeps::default();
        let mut pedestrian_gaps = Vec::new();
        if roundabout {
            pedestrian_gaps.extend(occupied.vehicles.iter().copied().filter(|vehicle| {
                vehicle.footprint.is_some_and(|body| {
                    map.get(junction)
                        .and_then(|inter| inter.raw_turn(vehicle.crossing.turn))
                        .is_some_and(|turn| {
                            body.middle()
                                .is_close(turn.points.first(), vehicle.owner.half_extent + GAP)
                        })
                })
            }));
        }
        if roundabout {
            // While a crossing group is on the zebra, close it to new starts
            // once a vehicle is waiting nearby. The group keeps moving, and
            // the next driver can use the first gap it leaves.
            for &(_, _, _, since, owner, ref request) in &eligible {
                if !matches!(owner, EntryOwner::Vehicle(_))
                    || request.speed > 0.2
                    || request.boundary.distance > PEDESTRIAN_GAP_APPROACH
                    || tick - since < PEDESTRIAN_GAP_AGE
                {
                    continue;
                }
                let sweep = Sweep::pending(request.user, request.crossing, request.speed);
                if occupied.walkers.iter().any(|&walker| {
                    !matches!(walker.remaining, Some((_, [])))
                        && crosswalk_gap_conflicts(map, walker, sweep, &mut work)
                }) {
                    pedestrian_gaps.push(sweep);
                }
            }
        }
        for (unaged, _, _, _, owner, request) in eligible {
            let sweep = Sweep::pending(request.user, request.crossing, request.speed);
            if matches!(owner, EntryOwner::Vehicle(_)) {
                match receiving_room(map, &request, vehicles, &mut work) {
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
            if roundabout
                && matches!(owner, EntryOwner::Vehicle(_))
                && request.speed <= 0.2
                && request.boundary.distance <= PEDESTRIAN_GAP_APPROACH
                && tick - waiting[&owner].eligible_since >= PEDESTRIAN_GAP_AGE
            {
                // A queued driver gets a gap between crossing groups.
                // Walkers already on the crossing keep their protection.
                pedestrian_gaps.push(sweep);
            }
            if !request.boundary.reached_by(request.speed * DELTA) {
                continue;
            }
            if aged_holds
                .conflict_owner(map, sweep, roundabout, &mut work)
                .is_some()
                || (roundabout
                    && matches!(owner, EntryOwner::Human(_))
                    && pedestrian_gaps
                        .iter()
                        .any(|&vehicle| crosswalk_gap_conflicts(map, sweep, vehicle, &mut work)))
            {
                self.denied.insert(owner, HoldReason::Conflict);
                continue;
            }
            let recovery_conflict = matches!(owner, EntryOwner::Vehicle(_))
                && recoveries
                    .iter()
                    .any(|recovery| recovery_conflicts(map, sweep, recovery, &mut work));
            let conflict = if recovery_conflict {
                None
            } else {
                occupied.conflict_owner(map, sweep, roundabout, &mut work)
            };
            if recovery_conflict || conflict.is_some() {
                self.denied.insert(owner, HoldReason::Conflict);
                if !unaged {
                    if !roundabout || matches!(owner, EntryOwner::Human(_)) {
                        aged_holds.insert(
                            Sweep {
                                waiting_at_curb: roundabout,
                                ..sweep
                            },
                            roundabout,
                        );
                    }
                }
            } else {
                occupied.insert(sweep, roundabout);
                self.grants.insert(owner, sweep.crossing);
            }
        }
    }

    pub(crate) fn permits_recovery(&self, owner: EntryOwner, junction: IntersectionID) -> bool {
        self.recoveries.get(&owner) == Some(&junction)
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
        if state.crossings.len() >= MAX_REQUESTS && !state.crossings.contains_key(&owner) {
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
    vehicles: &[&Actor<'_>],
    work: &mut usize,
) -> Option<bool> {
    let lane = map.get(request.crossing.to_lane())?;
    let needed = request.user.half_extent * 2.0 + GAP;
    if lane.points.length() < needed {
        return Some(false);
    }
    let start = lane.points.first();
    for actor in vehicles {
        *work = work.checked_sub(1)?;
        if actor.owner.owner == request.user.owner
            || (map.is_yield_roundabout(request.crossing.turn.parent)
                && (actor.speed > 0.5
                    || actor.itinerary.get_travers().is_none_or(|step| {
                        step.kind != TraverseKind::Lane(request.crossing.to_lane())
                    })))
            || !start
                .xy()
                .is_close(actor.position.xy(), needed + actor.owner.half_extent + 3.0)
        {
            continue;
        }
        // Model the footprint as an oriented capsule. Sweep only the storage
        // needed from the lane entrance and retain the deck separation.
        let body = actor.direction.xy().z0() * (actor.owner.half_extent - VEHICLE_BODY_RADIUS);
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
            let extent = body.dot(tangent).abs() + VEHICLE_BODY_RADIUS;
            // The entrance and its far edge are transverse cuts. Side
            // clearance must not inflate a bumper's longitudinal extent.
            if center + extent >= 0.0 && center - extent < storage_length {
                let end = segment.src + tangent * storage_length;
                let storage = Segment3::new(scale(segment.src), scale(end));
                if segments_close(
                    vehicle,
                    storage,
                    (VEHICLE_BODY_RADIUS + lane.kind.width() * 0.5).powi(2),
                ) {
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
    let Some(terminal) = actor.itinerary.local_path().first() else {
        return false;
    };
    if follows_walking_turn(map, *current, crossing) {
        let entry = lane.get_inter_node_pos(crossing.turn.parent);
        return terminal.is_close(entry, 0.05)
            && current.raw_points(map).is_some_and(|points| {
                points.len() <= MAX_PATH_POINTS
                    && points.project_dist2(actor.position) <= 0.8f32.powi(2)
            });
    }
    current.kind.is_lane()
        && terminal.is_close(endpoint, 0.05)
        && lane.points.project(actor.position).is_close(actor.position, 0.8)
}

fn still_occupies(map: &Map, crossing: Crossing, actor: &Actor<'_>) -> bool {
    let Some(turn) = map
        .get(crossing.turn.parent)
        .and_then(|inter| inter.raw_turn(crossing.turn))
    else {
        return false;
    };
    if actor.itinerary.is_pedestrian_egress() {
        return false;
    }
    let proper_mode = match actor.owner.owner {
        EntryOwner::Human(_) => {
            turn.kind == TurnKind::Crosswalk
                || (turn.kind == TurnKind::WalkingCorner
                    && !map.is_yield_roundabout(crossing.turn.parent))
        }
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
    if Crossing::from_traversal(current) == Some(crossing) {
        return turn.points.project(position).distance(position) <= half_extent + GAP;
    }
    let after_crosswalk =
        turn.kind == TurnKind::Crosswalk && follows_crosswalk_onto_corner(map, crossing, current);
    if !current.kind.is_lane() && !after_crosswalk && !follows_walking_turn(map, current, crossing)
    {
        return false;
    }
    if !after_crosswalk && current.destination_lane() == crossing.from_lane() {
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
    if !after_crosswalk
        && (current.destination_lane() != crossing.to_lane()
            || current.destination_intersection(map.lanes()) == Some(crossing.turn.parent))
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

fn follows_crosswalk_onto_corner(map: &Map, crossing: Crossing, current: Traversable) -> bool {
    Crossing::from_traversal(current).is_some_and(|corner| {
        corner.turn.parent == crossing.turn.parent
            && corner.from_lane() == crossing.to_lane()
            && turn_kind(map, corner.turn) == Some(TurnKind::WalkingCorner)
    })
}

fn recovery_can_move(
    map: &Map,
    junction: IntersectionID,
    recovery: &Recovery<'_>,
    vehicles: &[&Actor<'_>],
    work: &mut usize,
) -> bool {
    let mut position = recovery.position;
    let mut distance = recovery.speed.max(0.0) * DELTA;
    for &next in recovery.path.iter().rev() {
        let offset = next - position;
        let movement = Segment3::new(position, position + offset.cap_magnitude(distance));
        for vehicle in vehicles {
            if *work == 0 {
                return false;
            }
            *work -= 1;
            let body = vehicle.direction * (vehicle.owner.half_extent - VEHICLE_BODY_RADIUS);
            let footprint = Segment3::new(vehicle.position - body, vehicle.position + body);
            // Pad the current footprint by the distance the car could move
            // before the next barrier, even if its path bends.
            let radius = recovery.owner.half_extent + VEHICLE_BODY_RADIUS;
            let mut travel = vehicle.speed.max(0.0) * DELTA;
            if !segments_close(movement, footprint, (radius + travel).powi(2)) {
                continue;
            }
            if let Some(boundary) = vehicle.boundary(map).filter(|boundary| {
                boundary.controlled
                    && boundary.valid
                    && !boundary.overlaps
                    && boundary.crossing.turn.parent == junction
                    && boundary.reached_by(travel)
            }) {
                let sweep = Sweep::pending(vehicle.owner, boundary.crossing, vehicle.speed);
                if recovery_conflicts(map, sweep, recovery, work) {
                    // The same recovery sweep will stop this approach at its hold point.
                    travel = boundary.distance;
                }
            }
            if !segments_close(movement, footprint, (radius + travel).powi(2)) {
                continue;
            }
            // A walker already in contact can step away from a stopped car.
            // At equal distance along the body, only permit steps towards its end.
            let away = movement.src - footprint.project(movement.src);
            let step = movement.vec();
            let ending_gap = movement.dst.distance2(footprint.project(movement.dst));
            if travel > HOLD_TOLERANCE
                || away.dot(step) < 0.0
                || (ending_gap <= away.mag2() && (movement.src - vehicle.position).dot(step) <= 0.0)
            {
                return false;
            }
        }
        distance -= offset.mag();
        if distance <= 0.0 {
            break;
        }
        position = next;
    }
    true
}

fn remaining_segments(position: Vec3, path: &[Vec3]) -> impl Iterator<Item = Segment3> + '_ {
    let next = path.iter().rev().copied();
    let stationary = path.is_empty().then_some(Segment3::new(position, position));
    stationary.into_iter().chain(
        std::iter::once(position)
            .chain(next.clone())
            .zip(next)
            .map(|(a, b)| Segment3::new(a, b)),
    )
}

fn recovery_conflicts(
    map: &Map,
    vehicle: Sweep<'_>,
    recovery: &Recovery<'_>,
    work: &mut usize,
) -> bool {
    let Some(turn) = map
        .get(vehicle.crossing.turn.parent)
        .and_then(|inter| inter.raw_turn(vehicle.crossing.turn))
    else {
        return true;
    };
    if !map.is_yield_roundabout(vehicle.crossing.turn.parent) {
        return sweep_conflicts(
            turn.points.segments(),
            || remaining_segments(recovery.position, recovery.path),
            (vehicle.owner.half_extent + recovery.owner.half_extent + GAP).powi(2),
            work,
        );
    }
    let clearance2 = (VEHICLE_BODY_RADIUS + recovery.owner.half_extent + GAP).powi(2);
    let reach = vehicle.owner.half_extent * 2.0 + GAP + vehicle.speed.powi(2) / 12.0;
    sweep_conflicts(
        prefix(turn.points.segments(), reach),
        || remaining_segments(recovery.position, recovery.path),
        clearance2,
        work,
    )
}

fn prefix(
    segments: impl Iterator<Item = Segment3>,
    mut distance: f32,
) -> impl Iterator<Item = Segment3> {
    segments.map_while(move |segment| {
        if distance <= 0.0 {
            return None;
        }
        let offset = segment.vec();
        let dst = segment.src + offset.cap_magnitude(distance);
        distance -= offset.mag();
        Some(Segment3::new(segment.src, dst))
    })
}

pub(super) fn roundabout_merge_index(
    turn: &crate::map::Turn,
    ring: crate::map::RoundaboutGeometry,
) -> usize {
    let points = turn.points.as_slice();
    (1..points.len())
        .find(|&i| (points[i].xy().distance(ring.center.xy()) - ring.radius).abs() < 0.025)
        .unwrap_or(points.len() - 1)
}

struct SweepCursor<I: Iterator<Item = Vec3>> {
    points: std::iter::Peekable<I>,
    position: Vec3,
    direction: Vec3,
}

impl<I: Iterator<Item = Vec3>> SweepCursor<I> {
    fn new(points: I, position: Vec3, direction: Vec3) -> Self {
        Self {
            points: points.peekable(),
            position,
            direction,
        }
    }

    fn advance(&mut self, mut distance: f32, body: f32) -> Segment3 {
        let rear = self.position - self.direction * body;
        while let Some(&point) = self.points.peek() {
            let offset = point - self.position;
            let length = offset.mag();
            if length > 0.0 {
                self.direction = offset / length;
            }
            if length > distance {
                self.position += self.direction * distance;
                break;
            }
            self.position = point;
            self.points.next();
            distance -= length;
        }
        Segment3::new(rear, self.position + self.direction * body)
    }
}

fn roundabout_apron_conflict(
    entry: &crate::map::Turn,
    other: &crate::map::Turn,
    first: Sweep<'_>,
    second: Sweep<'_>,
    work: &mut usize,
) -> bool {
    const APRON_SECONDS: f32 = 0.25;
    if *work == 0 {
        return true;
    }
    *work -= 1;
    let Some(direction) = entry.points.first_dir() else {
        return true;
    };
    let Some(other_direction) = other.points.first_dir() else {
        return true;
    };
    let position = entry.points.first() - direction * first.owner.half_extent;
    let (other_position, other_direction) = second.footprint.map_or(
        (
            other.points.first() - other_direction * second.owner.half_extent,
            other_direction,
        ),
        |body| {
            (
                body.middle(),
                body.vec().try_normalize().unwrap_or(other_direction),
            )
        },
    );
    let entry_path = entry.points.as_slice();
    let other_path = second
        .remaining
        .map_or(other.points.as_slice(), |(_, path)| path);
    let other_points = (0..other_path.len()).map(|index| {
        if second.remaining.is_some() {
            other_path[other_path.len() - 1 - index]
        } else {
            other_path[index]
        }
    });
    let mut entry_cursor = SweepCursor::new(entry_path.iter().copied(), position, direction);
    let mut other_cursor = SweepCursor::new(other_points, other_position, other_direction);
    let advance_speed = |speed: f32, extent: f32| {
        let target = if extent > super::VehicleKind::Car.width() * 0.5 {
            7.2
        } else {
            9.0
        };
        speed + (target - speed).clamp(-6.0 * APRON_SECONDS, 2.5 * APRON_SECONDS)
    };
    let first_body = first.owner.half_extent - VEHICLE_BODY_RADIUS;
    let second_body = second.owner.half_extent - VEHICLE_BODY_RADIUS;
    let clearance2 = (VEHICLE_BODY_RADIUS * 2.0 + GAP).powi(2);
    if second.remaining.is_none() && first.crossing.from_lane() == second.crossing.from_lane() {
        return true;
    }
    // The entry reservation covers the immediate movement across the
    // zebra. The road controller arbitrates the later merge at the ring.
    let speed = advance_speed(first.speed, first.owner.half_extent);
    let other_speed = if second.footprint.is_none() {
        advance_speed(second.speed, second.owner.half_extent)
    } else {
        second.speed
    };
    let first_segment = entry_cursor.advance(speed * APRON_SECONDS, first_body);
    let second_segment = other_cursor.advance(other_speed * APRON_SECONDS, second_body);
    segments_close(first_segment, second_segment, clearance2)
}

fn crosswalk_gap_conflicts(
    map: &Map,
    walker: Sweep<'_>,
    vehicle: Sweep<'_>,
    work: &mut usize,
) -> bool {
    let Some(intersection) = map.get(walker.crossing.turn.parent) else {
        return true;
    };
    let Some((walk, drive)) = intersection
        .raw_turn(walker.crossing.turn)
        .zip(intersection.raw_turn(vehicle.crossing.turn))
    else {
        return true;
    };
    let clearance = walker.owner.half_extent + VEHICLE_BODY_RADIUS + GAP;
    sweep_conflicts(
        walk.points.segments(),
        || {
            prefix(
                drive.points.segments(),
                vehicle.owner.half_extent * 2.0 + GAP,
            )
        },
        clearance.powi(2),
        work,
    )
}

fn crosswalk_launch(turn: &crate::map::Turn, crossing: Crossing) -> Segment3 {
    const LAUNCH_DISTANCE: f32 = 2.0;
    let (from, to) = if crossing.backwards {
        (turn.points.last(), turn.points.first())
    } else {
        (turn.points.first(), turn.points.last())
    };
    Segment3::new(from, from + (to - from).cap_magnitude(LAUNCH_DISTANCE))
}

fn roundabout_conflicts(map: &Map, first: Sweep<'_>, second: Sweep<'_>, work: &mut usize) -> bool {
    if *work == 0 {
        return true;
    }
    *work -= 1;
    if first.owner.owner == second.owner.owner {
        return false;
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
    let first_is_vehicle = matches!(first.owner.owner, EntryOwner::Vehicle(_));
    let second_is_vehicle = matches!(second.owner.owner, EntryOwner::Vehicle(_));
    match (first_is_vehicle, second_is_vehicle) {
        (true, true) => {
            if first.speed * DELTA > first.owner.half_extent {
                return conflicts(map, first, second, work);
            }
            roundabout_apron_conflict(a, b, first, second, work)
        }
        (true, false) => {
            let margin = GAP * (first.speed / 3.0).clamp(0.25, 1.0);
            let reach = first.owner.half_extent * 2.0 + margin;
            let clearance2 = (VEHICLE_BODY_RADIUS + second.owner.half_extent + margin).powi(2);
            if let Some((position, path)) = second.remaining {
                use super::VehicleKind;
                let kind = if first.owner.half_extent == VehicleKind::Car.width() * 0.5 {
                    VehicleKind::Car
                } else if first.owner.half_extent == VehicleKind::Truck.width() * 0.5 {
                    VehicleKind::Truck
                } else {
                    VehicleKind::Bus
                };
                let speed = first.speed;
                let duration = 2.0 * reach
                    / (speed + (speed * speed + 2.0 * kind.acceleration() * reach).sqrt());
                return sweep_conflicts(
                    prefix(a.points.segments(), reach),
                    || {
                        prefix(
                            remaining_segments(position, path),
                            (second.speed * duration).max(0.01),
                        )
                    },
                    clearance2,
                    work,
                );
            }
            if second.waiting_at_curb {
                return sweep_conflicts(
                    prefix(a.points.segments(), reach),
                    || std::iter::once(crosswalk_launch(b, second.crossing)),
                    clearance2,
                    work,
                );
            }
            sweep_conflicts(
                prefix(a.points.segments(), reach),
                || b.points.segments(),
                clearance2,
                work,
            )
        }
        (false, true) => {
            let launch = crosswalk_launch(a, first.crossing);
            let clearance2 = (first.owner.half_extent + VEHICLE_BODY_RADIUS + GAP).powi(2);
            if let Some(footprint) = second.footprint {
                if sweep_conflicts(
                    std::iter::once(launch),
                    || std::iter::once(footprint),
                    clearance2,
                    work,
                ) {
                    return true;
                }
            }
            if let Some((position, path)) = second.remaining {
                let reach = second.speed.powi(2) / 12.0 + second.owner.half_extent + GAP;
                return sweep_conflicts(
                    std::iter::once(launch),
                    || prefix(remaining_segments(position, path), reach),
                    clearance2,
                    work,
                );
            }
            let reach = second.owner.half_extent * 2.0 + GAP;
            sweep_conflicts(
                std::iter::once(launch),
                || prefix(b.points.segments(), reach),
                clearance2,
                work,
            )
        }
        (false, false) => false,
    }
}

fn conflicts(map: &Map, first: Sweep<'_>, second: Sweep<'_>, work: &mut usize) -> bool {
    if *work == 0 {
        return true;
    }
    *work -= 1;
    if first.owner.owner == second.owner.owner {
        return false;
    }
    if (second.remaining.is_none() && first.crossing.from_lane() == second.crossing.from_lane())
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
    if let Some((position, path)) = second.remaining {
        sweep_conflicts(
            remaining_segments(position, path),
            || a.points.segments(),
            clearance2,
            work,
        )
    } else {
        sweep_conflicts(
            a.points.segments(),
            || b.points.segments(),
            clearance2,
            work,
        )
    }
}

fn sweep_conflicts<Other: Iterator<Item = Segment3>>(
    segments: impl Iterator<Item = Segment3>,
    other: impl Fn() -> Other,
    clearance2: f32,
    work: &mut usize,
) -> bool {
    for a in segments {
        for b in other() {
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

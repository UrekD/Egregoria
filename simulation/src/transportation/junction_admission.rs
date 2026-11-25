use crate::map::{
    ApproachRole, IntersectionID, LaneID, LightPolicy, Map, Traversable, TraverseDirection,
    TraverseKind, TurnID,
};
use crate::map_dynamic::Itinerary;
use crate::transportation::Location;
use crate::world_command::bounded_command_vec;
use crate::{HumanID, HumanWork, VehicleID, World};
use geom::{Segment3, Vec3};
use prototypes::{game_secs_to_ticks, DELTA};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_REQUESTS: usize = 4096;
const MAX_OWNERS_VISITED: usize = 65_536;
const MAX_PATH_POINTS: usize = 256;
const MAX_TURN_SEGMENT_PAIRS: usize = 16_384;
const MAX_RECEIVING_OCCUPANTS: usize = 64;
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
        tick: u64,
    ) -> Result<(), &'static str> {
        if self.waiting.values().any(|clock| clock.seen > tick) {
            return Err("junction admission clock is ahead of the simulation");
        }
        let owner_exists = |owner| match owner {
            EntryOwner::Vehicle(id) => world.vehicles.contains_key(id),
            EntryOwner::Human(id) => world.humans.get(id).is_some(),
        };
        let valid_crossing = |crossing: Crossing| {
            (!crossing.backwards || crossing.turn.bidirectional)
                && map.allows_turn(crossing.turn)
                && configured(map, crossing.turn.parent).is_some()
        };
        // A lifecycle system can retire the saved owner after the movement pass.
        self.waiting
            .retain(|&owner, clock| owner_exists(owner) && valid_crossing(clock.crossing));
        self.crossings
            .retain(|&owner, &mut crossing| owner_exists(owner) && valid_crossing(crossing));
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

struct EntryRequest {
    user: MovingOwner,
    crossing: Crossing,
    role: ApproachRole,
    distance: f32,
    speed: f32,
}

#[derive(Clone, Copy)]
struct LaneOccupier {
    user: EntryOwner,
    rear: f32,
}

#[derive(Clone, Copy)]
struct Sweep {
    owner: MovingOwner,
    crossing: Crossing,
}

struct Actor<'a> {
    owner: MovingOwner,
    itinerary: &'a Itinerary,
    position: Vec3,
    speed: f32,
}

#[derive(Default)]
pub(crate) struct JunctionFrame {
    occupied: BTreeMap<IntersectionID, Vec<Sweep>>,
    requests: Vec<EntryRequest>,
    receiving: BTreeMap<LaneID, Vec<LaneOccupier>>,
    grants: BTreeMap<EntryOwner, Crossing>,
    denied: BTreeMap<EntryOwner, HoldReason>,
    overflow: bool,
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
        overlaps: distance.is_some_and(|distance| distance < -0.001),
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
        world: &mut World,
        map: &Map,
        state: &mut JunctionAdmission,
        tick: u64,
    ) -> Self {
        let mut frame = Self::default();
        if !map.has_junction_controller() {
            state.waiting.clear();
            state.crossings.clear();
            return frame;
        }
        if world.vehicles.len().saturating_add(world.humans.len()) > MAX_OWNERS_VISITED {
            frame.overflow = true;
            return frame;
        }
        let mut human_ids = Vec::new();
        world.collect_active(HumanWork::Itinerary, &mut human_ids);
        let mut retained = BTreeMap::new();
        for (id, vehicle) in &world.vehicles {
            if vehicle.collider.is_none() || vehicle.vehicle.state.is_parked() {
                continue;
            }
            frame.observe(
                map,
                state,
                &mut retained,
                Actor {
                    owner: MovingOwner {
                        owner: EntryOwner::Vehicle(id),
                        half_extent: vehicle.vehicle.kind.width() * 0.5,
                    },
                    itinerary: &vehicle.it,
                    position: vehicle.trans.pos,
                    speed: vehicle.speed.0,
                },
            );
        }
        for id in human_ids {
            let human = &world.humans[id];
            if human.location != Location::Outside {
                continue;
            }
            frame.observe(
                map,
                state,
                &mut retained,
                Actor {
                    owner: MovingOwner {
                        owner: EntryOwner::Human(id),
                        half_extent: 0.3,
                    },
                    itinerary: &human.it,
                    position: human.trans.pos,
                    speed: human.speed.0,
                },
            );
        }
        if frame.overflow {
            return frame;
        }
        state.crossings = retained;
        frame.reserve(map, state, tick);
        frame
    }

    fn observe(
        &mut self,
        map: &Map,
        state: &JunctionAdmission,
        retained: &mut BTreeMap<EntryOwner, Crossing>,
        actor: Actor<'_>,
    ) {
        if self.overflow {
            return;
        }
        let Actor {
            owner,
            itinerary,
            position,
            speed,
        } = actor;
        let current = itinerary.get_travers().copied();
        let boundary = entry_boundary(map, itinerary, position, speed * DELTA, owner.half_extent);
        let active_turn = current
            .and_then(Crossing::from_traversal)
            .filter(|crossing| configured(map, crossing.turn.parent).is_some());
        let initial_nose = boundary
            .as_ref()
            .filter(|boundary| boundary.overlaps && boundary.valid && boundary.controlled)
            .map(|boundary| boundary.crossing);
        let previous = state
            .crossings
            .get(&owner.owner)
            .copied()
            .filter(|crossing| {
                still_occupies(map, *crossing, current, position, owner.half_extent)
            });
        if let Some(crossing) = active_turn.or(previous).or(initial_nose) {
            self.occupied
                .entry(crossing.turn.parent)
                .or_default()
                .push(Sweep { owner, crossing });
            retained.insert(owner.owner, crossing);
            self.overflow = retained.len() > MAX_REQUESTS;
        }

        if matches!(owner.owner, EntryOwner::Vehicle(_)) {
            if let Some(Traversable {
                kind: TraverseKind::Lane(id),
                ..
            }) = current
            {
                if let Some(lane) = map
                    .get(id)
                    .filter(|lane| configured(map, lane.src).is_some())
                {
                    let near = lane.points.first().distance(position);
                    if near < 24.0 {
                        let projection = lane.points.project(position);
                        if projection.distance(position) <= owner.half_extent + GAP {
                            let offset = lane.points.first().distance(projection);
                            let users = self.receiving.entry(id).or_default();
                            if users.len() <= MAX_RECEIVING_OCCUPANTS {
                                users.push(LaneOccupier {
                                    user: owner.owner,
                                    rear: offset - owner.half_extent,
                                });
                            }
                        }
                    }
                }
            }
        }
        let Some(boundary) = boundary else {
            return;
        };
        if !boundary.valid || !boundary.controlled || !boundary.bounded {
            return;
        }
        let crossing = boundary.crossing;
        if retained.get(&owner.owner) == Some(&crossing) {
            return;
        }
        let role = match owner.owner {
            EntryOwner::Human(_) => ApproachRole::Yield,
            EntryOwner::Vehicle(_) => {
                let Some(lane) = map.get(crossing.from_lane()) else {
                    return;
                };
                let junction = crossing.turn.parent;
                map.junction_rules(junction)
                    .and_then(|rules| rules.role(lane.parent))
                    .unwrap_or_else(|| {
                        if configured(map, junction) == Some(LightPolicy::StopSigns) {
                            ApproachRole::Stop
                        } else {
                            ApproachRole::Yield
                        }
                    })
            }
        };
        if self.requests.len() == MAX_REQUESTS {
            self.overflow = true;
            return;
        }
        self.requests.push(EntryRequest {
            user: owner,
            crossing,
            role,
            distance: boundary.distance,
            speed,
        });
    }

    fn reserve(&mut self, map: &Map, state: &mut JunctionAdmission, tick: u64) {
        let mut eligible = Vec::new();
        let mut waiting = BTreeMap::new();
        let mut heads = BTreeMap::new();
        for request in &self.requests {
            if matches!(request.user.owner, EntryOwner::Vehicle(_)) {
                let candidate = (request.distance, request.user.owner);
                let head = heads
                    .entry(request.crossing.from_lane())
                    .or_insert(candidate);
                if candidate < *head {
                    *head = candidate;
                }
            }
        }
        for request in self.requests.drain(..) {
            let owner = request.user.owner;
            let crossing = request.crossing;
            let junction = crossing.turn.parent;
            let needed_storage = request.user.half_extent * 2.0 + GAP;
            let receives = match owner {
                EntryOwner::Vehicle(_) => map.get(crossing.to_lane()).is_some_and(|lane| {
                    lane.points.length() >= needed_storage
                        && self.receiving.get(&crossing.to_lane()).is_none_or(|users| {
                            users.len() <= MAX_RECEIVING_OCCUPANTS
                                && users.iter().all(|other| {
                                    other.user == owner || other.rear >= needed_storage
                                })
                        })
                }),
                EntryOwner::Human(_) => true,
            };
            if matches!(owner, EntryOwner::Vehicle(_)) && heads[&crossing.from_lane()].1 != owner {
                self.denied.insert(owner, HoldReason::Conflict);
                continue;
            }
            let at_hold = request.distance <= HOLD_TOLERANCE;
            let clock = state.waiting.get(&owner).filter(|clock| {
                clock.crossing == crossing
                    && (clock.seen == tick || clock.seen.checked_add(1) == Some(tick))
            });
            let eligible_since = clock.map_or(tick, |clock| clock.eligible_since);
            let stopped_since = (at_hold && request.speed.abs() <= 0.2)
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

            let denial = if request.role == ApproachRole::Stop
                && !stopped_since.is_some_and(|since| tick.saturating_sub(since) >= STOP_DWELL)
            {
                Some(HoldReason::StopDwell)
            } else if !receives {
                Some(HoldReason::ExitFull)
            } else {
                None
            };
            if let Some(denial) = denial {
                self.denied.insert(owner, denial);
            } else if request.speed * DELTA > request.distance + 0.001 {
                let aged = tick.saturating_sub(eligible_since) >= AGE_PRIORITY;
                eligible.push((
                    junction,
                    !aged,
                    (!aged).then_some(request.role),
                    eligible_since,
                    owner,
                    Sweep {
                        owner: request.user,
                        crossing,
                    },
                ));
            }
        }
        state.waiting = waiting;
        eligible.sort_unstable_by_key(|&(junction, unaged, role, since, owner, _)| {
            (junction, unaged, role, since, owner)
        });
        let mut work = MAX_TURN_SEGMENT_PAIRS;
        let mut aged_holds: BTreeMap<IntersectionID, Vec<Sweep>> = BTreeMap::new();
        for (junction, unaged, _, _, owner, sweep) in eligible {
            let occupied = self.occupied.entry(junction).or_default();
            let holds = aged_holds.entry(junction).or_default();
            if holds
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
                    // Once the aged movement has receiving room, hold younger
                    // conflicting arrivals to let the occupied crossing clear.
                    holds.push(sweep);
                }
            } else {
                occupied.push(sweep);
                self.grants.insert(owner, sweep.crossing);
            }
        }
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
        if self.grants.get(&owner) != Some(&boundary.crossing) || self.overflow {
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

fn still_occupies(
    map: &Map,
    crossing: Crossing,
    current: Option<Traversable>,
    position: Vec3,
    half_extent: f32,
) -> bool {
    let Some(turn) = map
        .get(crossing.turn.parent)
        .and_then(|inter| inter.raw_turn(crossing.turn))
    else {
        return false;
    };
    if let Some(Traversable {
        kind: TraverseKind::Lane(id),
        ..
    }) = current
    {
        let endpoint = if (id == crossing.from_lane()) == crossing.backwards {
            turn.points.last()
        } else {
            turn.points.first()
        };
        if id == crossing.from_lane() || id == crossing.to_lane() {
            return endpoint.distance(position) <= half_extent + GAP;
        }
    }
    turn.points.project(position).distance(position) <= half_extent + GAP
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

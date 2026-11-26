use crate::map::{
    Lane, LaneID, Map, PathKind, Pathfinder, RoadID, Traversable, TraverseDirection, TraverseKind,
};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::ParkingManagement;
use crate::transportation::junction_admission::{
    entry_boundary, EntryOwner, HoldReason, JunctionAdmission, JunctionFrame, MovingOwner,
};
use crate::transportation::queue_diagnostics::{QueueDiagnostics, QueueHoldReason};
use crate::transportation::traffic_metrics::within_lane;
use crate::transportation::train::{can_move_train, record_train_motion, TrainReservations};
use crate::transportation::{
    put_pedestrian_in_transport_grid, Location, RoadThroughput, RoadUserKind, TransportGrid,
    VehicleKind,
};
use crate::utils::resources::Resources;
use crate::world::TrainID;
use crate::{HumanWork, World};
use egui_inspect::egui::Ui;
use egui_inspect::{Inspect, InspectArgs};
use geom::{Follower, PolyLine3, Polyline3Queue, Segment3, Vec3};
use prototypes::{game_secs_to_ticks, GameTime, Tick, DELTA};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;

#[path = "road_edit_lifecycle.rs"]
mod road_edit_lifecycle;
use road_edit_lifecycle::{
    exit_path_still_valid, needs_local_recovery, pedestrian_clearance, PedestrianClearance,
};

pub use pathpark_route::{DirectedEndpoint, DirectedPlan, DirectedSearch, DirectedSearchResult};

const EGRESS_RECHECK_TICKS: u64 = game_secs_to_ticks(30);
const MAX_EGRESS_CHECKS_PER_TICK: usize = 8;

#[derive(Inspect, Debug, Serialize, Deserialize)]
pub struct ItineraryFollower {
    pub leader: TrainID,
    #[inspect(skip)]
    pub head: Follower,
    #[inspect(skip)]
    pub tail: Follower,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ItineraryLeader {
    pub past: Polyline3Queue,
}

#[derive(Default, Debug, Inspect)]
pub struct Itinerary {
    kind: ItineraryKind,
    reversed_local_path: Vec<Vec3>,
    #[inspect(skip)]
    road_approach: RoadApproach,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
enum RoadApproach {
    #[default]
    Unchecked,
    Tracking(Option<RoadID>),
    Joining {
        lane: LaneID,
        kind: PathKind,
        previous: Option<RoadID>,
    },
}

impl RoadApproach {
    fn last_road(self) -> Option<RoadID> {
        match self {
            Self::Unchecked => None,
            Self::Tracking(road) | Self::Joining { previous: road, .. } => road,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub enum ItineraryKind {
    #[default]
    None,
    WaitUntil(f64),
    Simple(Vec3),
    Route(Route, PathKind),
    WaitForReroute {
        kind: PathKind,
        target: ItineraryTarget,
        wait_ticks: u16,
    },
    PedestrianEgress(PedestrianEgress),
    Driveway(crate::transportation::DrivewayPhase),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PedestrianEgress {
    crossing: Option<Traversable>,
    retry: Option<(ItineraryTarget, u16)>,
    check_at: Tick,
}

#[derive(Serialize, Deserialize)]
struct SavedItinerary<R, P> {
    kind: SavedItineraryKind<R>,
    reversed_local_path: P,
}

// Preserve the original variant positions for saves without road approach state.
// Only active routes and their retries need the extra state. The history itself
// stays in the transient RoadThroughput resource.
#[derive(Serialize, Deserialize)]
enum SavedItineraryKind<R> {
    None,
    WaitUntil(f64),
    Simple(Vec3),
    Route(R, PathKind),
    WaitForReroute {
        kind: PathKind,
        target: ItineraryTarget,
        wait_ticks: u16,
    },
    TrackedRoute(R, PathKind, RoadApproach),
    TrackedRetry {
        kind: PathKind,
        target: ItineraryTarget,
        wait_ticks: u16,
        previous: RoadID,
    },
    PedestrianEgress(PedestrianEgress, RoadApproach),
    Driveway(crate::transportation::DrivewayPhase),
}

impl Serialize for Itinerary {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use SavedItineraryKind as Saved;
        let kind = match self.kind {
            ItineraryKind::None => Saved::None,
            ItineraryKind::WaitUntil(time) => Saved::WaitUntil(time),
            ItineraryKind::Simple(end) => Saved::Simple(end),
            ItineraryKind::Route(ref route, kind) => match self.road_approach {
                RoadApproach::Unchecked => Saved::Route(route, kind),
                approach => Saved::TrackedRoute(route, kind, approach),
            },
            ItineraryKind::WaitForReroute {
                kind,
                target,
                wait_ticks,
            } => match self.road_approach.last_road() {
                Some(previous) => Saved::TrackedRetry {
                    kind,
                    target,
                    wait_ticks,
                    previous,
                },
                None => Saved::WaitForReroute {
                    kind,
                    target,
                    wait_ticks,
                },
            },
            ItineraryKind::Driveway(phase) => Saved::Driveway(phase),
            ItineraryKind::PedestrianEgress(egress) => {
                Saved::PedestrianEgress(egress, self.road_approach)
            }
        };
        SavedItinerary {
            kind,
            reversed_local_path: self.local_path(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Itinerary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use SavedItineraryKind as Saved;
        let SavedItinerary {
            kind,
            reversed_local_path,
        }: SavedItinerary<Route, Vec<Vec3>> = SavedItinerary::deserialize(deserializer)?;
        let mut road_approach = RoadApproach::Unchecked;
        let kind = match kind {
            Saved::None => ItineraryKind::None,
            Saved::WaitUntil(time) => ItineraryKind::WaitUntil(time),
            Saved::Simple(end) => ItineraryKind::Simple(end),
            Saved::Route(route, kind) => ItineraryKind::Route(route, kind),
            Saved::WaitForReroute {
                kind,
                target,
                wait_ticks,
            } => ItineraryKind::WaitForReroute {
                kind,
                target,
                wait_ticks,
            },
            Saved::TrackedRoute(route, kind, approach) => {
                road_approach = approach;
                ItineraryKind::Route(route, kind)
            }
            Saved::TrackedRetry {
                kind,
                target,
                wait_ticks,
                previous,
            } => {
                road_approach = RoadApproach::Tracking(Some(previous));
                ItineraryKind::WaitForReroute {
                    kind,
                    target,
                    wait_ticks,
                }
            }
            Saved::Driveway(phase) => {
                if !reversed_local_path.is_empty() {
                    return Err(serde::de::Error::custom("invalid driveway itinerary"));
                }
                ItineraryKind::Driveway(phase)
            }
            Saved::PedestrianEgress(egress, approach) => {
                let valid = if let Some(crossing) = egress.crossing {
                    (match crossing.kind {
                        TraverseKind::Lane(_) => reversed_local_path.len() == 1,
                        TraverseKind::Turn(_) => (1..=2).contains(&reversed_local_path.len()),
                    })
                        && reversed_local_path.iter().all(|point| point.is_finite())
                } else {
                    reversed_local_path.is_empty()
                };
                if !valid {
                    return Err(serde::de::Error::custom("invalid pedestrian egress path"));
                }
                road_approach = approach;
                ItineraryKind::PedestrianEgress(egress)
            }
        };
        Ok(Self {
            kind,
            reversed_local_path,
            road_approach,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ItineraryTarget {
    Position(Vec3),
    /// Offset in meters from the start of the driving lane.
    DirectedLane(LaneID, f32),
}

#[derive(Debug, Serialize, Deserialize, Inspect)]
pub struct Route {
    /// Route is reversed, allows for efficient popping
    pub reversed_route: Vec<Traversable>,
    pub end_pos: Vec3,
    pub cur: Traversable,
    #[inspect(skip)]
    target: ItineraryTarget,
}

pub const OBJECTIVE_OK_DIST: f32 = 3.0;
const REROUTE_WAIT_TICKS: u16 = 200;
const MAX_REROUTE_FAILURES: usize = 8_192;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct RerouteKey {
    kind: PathKind,
    start: [u32; 3],
    end: RerouteTarget,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum RerouteTarget {
    Position([u32; 3]),
    DirectedLane(LaneID, u32),
}

#[derive(Default)]
pub(crate) struct RerouteFailures {
    map_revision: (u64, u64, u64),
    entries: BTreeSet<RerouteKey>,
}

impl RerouteFailures {
    fn sync_map(&mut self, map: &Map) {
        let revision = (
            map.identity(),
            map.walking_topology_generation,
            map.driving_policy_generation,
        );
        if self.map_revision != revision {
            if (self.map_revision.0, self.map_revision.1) == (revision.0, revision.1) {
                self.entries.retain(|key| key.kind != PathKind::Vehicle);
            } else {
                self.entries.clear();
            }
            self.map_revision = revision;
        }
    }

    fn route(
        &mut self,
        tick: Tick,
        start: Vec3,
        target: ItineraryTarget,
        map: &Map,
        kind: PathKind,
    ) -> Option<Itinerary> {
        // Exact endpoints preserve same-lane direction checks and changes in origin.
        let key = RerouteKey {
            kind,
            start: <[f32; 3]>::from(start).map(f32::to_bits),
            end: match target {
                ItineraryTarget::Position(end) => {
                    RerouteTarget::Position(<[f32; 3]>::from(end).map(f32::to_bits))
                }
                ItineraryTarget::DirectedLane(lane, offset) => {
                    RerouteTarget::DirectedLane(lane, offset.to_bits())
                }
            },
        };
        if self.entries.contains(&key) {
            return None;
        }

        // The tick seeds vehicle and rail search costs. Reachability for these
        // endpoints depends on the lane geometry and turns in this map revision.
        let result = Itinerary::route_to_target(tick, start, target, map, kind);
        if result.is_none() {
            if self.entries.len() == MAX_REROUTE_FAILURES {
                self.entries.pop_first();
            }
            self.entries.insert(key);
        }
        result
    }
}

impl Itinerary {
    pub const NONE: Self = Self {
        kind: ItineraryKind::None,
        reversed_local_path: Vec::new(),
        road_approach: RoadApproach::Unchecked,
    };

    pub fn simple(mut path: Vec<Vec3>) -> Self {
        path.reverse();
        Self {
            kind: ItineraryKind::Simple(*path.last().unwrap()),
            reversed_local_path: path,
            ..Self::default()
        }
    }

    pub fn wait_until(x: f64) -> Self {
        Self {
            kind: ItineraryKind::WaitUntil(x),
            ..Self::default()
        }
    }

    pub fn is_wait_for_reroute(&self) -> Option<u16> {
        if let ItineraryKind::WaitForReroute { wait_ticks, .. } = self.kind {
            Some(wait_ticks)
        } else {
            None
        }
    }

    pub fn wait_for_reroute(kind: PathKind, dest: Vec3) -> Self {
        Self::retry(kind, ItineraryTarget::Position(dest))
    }

    pub fn wait_for_reroute_to_lane(lane: LaneID, offset: f32) -> Self {
        Self::retry(
            PathKind::Vehicle,
            ItineraryTarget::DirectedLane(lane, offset),
        )
    }

    fn retry(kind: PathKind, target: ItineraryTarget) -> Self {
        Self {
            kind: ItineraryKind::WaitForReroute {
                kind,
                target,
                wait_ticks: 0,
            },
            ..Self::default()
        }
    }

    fn retry_after_edit(kind: PathKind, target: ItineraryTarget, approach: RoadApproach) -> Self {
        Self {
            road_approach: RoadApproach::Tracking(approach.last_road()),
            ..Self::retry(kind, target)
        }
    }

    pub fn route(
        tick: Tick,
        start: Vec3,
        end: Vec3,
        map: &Map,
        pathkind: PathKind,
    ) -> Option<Itinerary> {
        Self::route_to_target(tick, start, ItineraryTarget::Position(end), map, pathkind)
    }

    pub fn route_to_lane(
        tick: Tick,
        start: Vec3,
        map: &Map,
        lane: LaneID,
        offset: f32,
    ) -> Option<Itinerary> {
        Self::route_to_target(
            tick,
            start,
            ItineraryTarget::DirectedLane(lane, offset),
            map,
            PathKind::Vehicle,
        )
    }

    pub(crate) fn route_between_lanes(
        tick: Tick,
        map: &Map,
        (start_lane, start_offset): (LaneID, f32),
        (end_lane, end_offset): (LaneID, f32),
    ) -> Option<Itinerary> {
        let start = driving_lane_points(map, start_lane, start_offset)?.point_along(start_offset);
        let target = ItineraryTarget::DirectedLane(end_lane, end_offset);
        Self::route_from(tick, start, start_lane, target, map, PathKind::Vehicle)
    }

    pub(crate) fn route_rail(
        tick: Tick,
        map: &Map,
        start: Vec3,
        start_lane: LaneID,
        end_lane: LaneID,
        offset: f32,
    ) -> Option<Self> {
        Self::route_from(
            tick,
            start,
            start_lane,
            ItineraryTarget::DirectedLane(end_lane, offset),
            map,
            PathKind::Rail,
        )
    }

    fn route_to_target(
        tick: Tick,
        start: Vec3,
        target: ItineraryTarget,
        map: &Map,
        pathkind: PathKind,
    ) -> Option<Itinerary> {
        #[cfg(test)]
        ROUTE_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));

        let start_lane = pathkind.nearest_lane(map, start)?;
        Self::route_from(tick, start, start_lane, target, map, pathkind)
    }

    fn route_from(
        tick: Tick,
        start: Vec3,
        start_lane: LaneID,
        target: ItineraryTarget,
        map: &Map,
        pathkind: PathKind,
    ) -> Option<Itinerary> {
        let (end_lane, end) = match target {
            ItineraryTarget::Position(end) => (pathkind.nearest_lane(map, end)?, end),
            ItineraryTarget::DirectedLane(lane, offset) => (
                lane,
                directed_lane_points(map, lane, offset, pathkind)?.point_along(offset),
            ),
        };

        let mut cur = Traversable::new(TraverseKind::Lane(start_lane), TraverseDirection::Forward);

        if start_lane == end_lane {
            let local_path = match target {
                ItineraryTarget::Position(_) => pathkind
                    .local_route(map, start_lane, start, end)
                    .map(PolyLine3::into_vec),
                ItineraryTarget::DirectedLane(lane, offset) => directed_local_path(
                    directed_lane_points(map, lane, offset, pathkind)?,
                    start,
                    offset,
                    end,
                ),
            };
            if let Some(mut local_path) = local_path {
                local_path.reverse();
                return Some(Itinerary {
                    kind: ItineraryKind::Route(
                        Route {
                            reversed_route: vec![],
                            end_pos: end,
                            cur,
                            target,
                        },
                        pathkind,
                    ),
                    reversed_local_path: local_path,
                    ..Self::default()
                });
            }
        }

        let mut reversed_route: Vec<Traversable> = pathkind
            .path(map, tick, cur, end_lane)?
            .into_iter()
            .rev()
            .collect();

        reversed_route.pop(); // Remove start

        if let Some(&Traversable {
            kind: TraverseKind::Lane(id),
            ..
        }) = reversed_route.last()
        {
            #[allow(clippy::unwrap_used)] // just checked that last is some
            if id == start_lane {
                cur = reversed_route.pop().unwrap();
            }
        }

        let kind = ItineraryKind::Route(
            Route {
                reversed_route,
                end_pos: end,
                cur,
                target,
            },
            pathkind,
        );

        let points = cur.points(map)?;
        let (proj, segid, dir) = points.project_segment_dir(start);

        let mut points = points.into_vec();
        points.drain(..segid);
        points.reverse();

        let mut it = Self {
            kind,
            reversed_local_path: points,
            ..Self::default()
        };
        if matches!(pathkind, PathKind::Rail) {
            return Some(it);
        }

        it.prepend_local_path([proj + dir * 3.5].iter().copied());
        Some(it)
    }

    fn advance(&mut self, map: &Map, position: Vec3, track_road_entries: bool) -> Option<Vec3> {
        let v = self.reversed_local_path.pop();

        if self.reversed_local_path.is_empty() {
            if let ItineraryKind::Route(ref mut r, pathkind) = self.kind {
                r.cur = r.reversed_route.pop()?;

                let points = match r.cur.points(map) {
                    Some(x) => x,
                    None => {
                        *self = Self::retry_after_edit(pathkind, r.target, self.road_approach);
                        return None;
                    }
                };

                if r.reversed_route.is_empty() {
                    self.reversed_local_path = match r.target {
                        ItineraryTarget::Position(_) => pathkind
                            .local_route(map, r.cur.destination_lane(), position, r.end_pos)
                            .unwrap_or(points)
                            .into_vec(),
                        ItineraryTarget::DirectedLane(lane, offset) => {
                            let path = directed_lane_points(map, lane, offset, pathkind).and_then(
                                |points| {
                                    directed_local_path(points, points.first(), offset, r.end_pos)
                                },
                            );
                            let Some(path) = path else {
                                *self =
                                    Self::retry_after_edit(pathkind, r.target, self.road_approach);
                                return None;
                            };
                            path
                        }
                    };
                } else {
                    self.reversed_local_path = points.into_vec();
                }
                self.reversed_local_path.reverse();

                if track_road_entries && pathkind != PathKind::Rail {
                    let previous = self.road_approach.last_road();
                    self.road_approach = match r.cur.kind {
                        TraverseKind::Lane(id)
                            if map.get(id).is_some_and(|lane| {
                                pathkind.authorized_lane(lane.kind) && Some(lane.parent) != previous
                            }) =>
                        {
                            RoadApproach::Joining {
                                lane: id,
                                kind: pathkind,
                                previous,
                            }
                        }
                        // A path can cross several junction turns before the next lane.
                        _ => RoadApproach::Tracking(previous),
                    };
                }
            }
        }
        v
    }

    fn initial_road_approach(
        &self,
        map: &Map,
        position: Vec3,
        previous: Option<RoadID>,
    ) -> RoadApproach {
        let ItineraryKind::Route(route, kind) = &self.kind else {
            return RoadApproach::Tracking(None);
        };
        if *kind == PathKind::Rail {
            return RoadApproach::Tracking(None);
        }
        let id = match route.cur.kind {
            TraverseKind::Lane(id) => id,
            TraverseKind::Turn(turn) => {
                // Older saves have no road approach state. Recover the origin
                // of the active turn from its direction.
                let origin = match route.cur.dir {
                    TraverseDirection::Forward => turn.src,
                    TraverseDirection::Backward => turn.dst,
                };
                return RoadApproach::Tracking(
                    map.get(origin)
                        .filter(|lane| kind.authorized_lane(lane.kind))
                        .map(|lane| lane.parent),
                );
            }
        };
        let Some(lane) = map.get(id).filter(|lane| kind.authorized_lane(lane.kind)) else {
            return RoadApproach::Tracking(None);
        };
        let Some(road) = map.get(lane.parent) else {
            return RoadApproach::Tracking(None);
        };

        // A route can start in a driveway, a parking bay or next to a sidewalk.
        // A reroute from any travel lane of this same segment is already on it.
        if previous == Some(road.id)
            || (previous.is_none()
                && road.lanes_iter().any(|(id, lane_kind)| {
                    kind.authorized_lane(lane_kind)
                        && map.get(id).is_some_and(|lane| within_lane(lane, position))
                }))
        {
            RoadApproach::Tracking(Some(road.id))
        } else {
            RoadApproach::Joining {
                lane: id,
                kind: *kind,
                previous,
            }
        }
    }

    fn observe_road_entry(
        &mut self,
        from: Vec3,
        to: Vec3,
        map: &Map,
        on_road_entry: &mut Option<impl FnMut(RoadID, PathKind)>,
    ) {
        if let (RoadApproach::Joining { lane, kind, .. }, Some(record)) =
            (self.road_approach, on_road_entry)
        {
            if let Some(lane) = map
                .get(lane)
                .filter(|lane| movement_reaches_lane(lane, from, to))
            {
                record(lane.parent, kind);
                self.road_approach = RoadApproach::Tracking(Some(lane.parent));
            }
        }
    }

    pub fn update(
        &mut self,
        position: Vec3,
        dist_to_move: f32,
        tick: Tick,
        time: u32,
        map: &Map,
    ) -> Vec3 {
        self.update_with_route(
            position,
            dist_to_move,
            time,
            map,
            |start, target, kind| Self::route_to_target(tick, start, target, map, kind),
            None::<fn(RoadID, PathKind)>,
        )
    }

    fn update_with_route(
        &mut self,
        position: Vec3,
        dist_to_move: f32,
        time: u32,
        map: &Map,
        route: impl FnOnce(Vec3, ItineraryTarget, PathKind) -> Option<Self>,
        on_road_entry: Option<impl FnMut(RoadID, PathKind)>,
    ) -> Vec3 {
        self.update_guarded::<false>(
            position,
            dist_to_move,
            time,
            map,
            route,
            on_road_entry,
            None,
            |_, _, _| true,
            None,
        )
    }

    fn update_guarded<const DRIVEWAYS: bool>(
        &mut self,
        mut position: Vec3,
        mut dist_to_move: f32,
        time: u32,
        map: &Map,
        route: impl FnOnce(Vec3, ItineraryTarget, PathKind) -> Option<Self>,
        mut on_road_entry: Option<impl FnMut(RoadID, PathKind)>,
        mut admission: Option<&mut MovementAdmission<'_>>,
        mut move_on: impl FnMut(Traversable, Vec3, Vec3) -> bool,
        mut driveway: Option<DrivewayMovement<'_>>,
    ) -> Vec3 {
        if self.is_pedestrian_egress() {
            return self.advance_egress::<DRIVEWAYS>(
                map,
                position,
                dist_to_move,
                driveway.as_mut(),
            );
        }
        if on_road_entry.is_some()
            && matches!(self.road_approach, RoadApproach::Unchecked)
            && !self.reversed_local_path.is_empty()
        {
            self.road_approach = self.initial_road_approach(map, position, None);
        }
        if let ItineraryKind::Route(ref active, kind) = self.kind {
            if matches!(active.target, ItineraryTarget::DirectedLane(..))
                && active.cur.raw_points(map).is_none()
            {
                *self = Self::retry_after_edit(kind, active.target, self.road_approach);
            }
        }
        let mut on_initial_approach = true;
        while let Some(p) = self.get_point() {
            let dist = position.distance(p);
            let mut snap = 0.01;
            if let Some(admission) = &mut admission {
                if let Some(boundary) =
                    entry_boundary(map, self, position, dist_to_move, admission.user.half_extent)
                {
                    if boundary.reached_by(dist) {
                        snap = 0.0;
                    }
                    if boundary.reached_by(dist_to_move) {
                        if let Err(reason) = admission.frame.try_enter(
                            admission.state,
                            admission.user.owner,
                            &boundary,
                            on_initial_approach,
                        ) {
                            if boundary.distance <= dist + 0.001 {
                                let next = if boundary.distance > 0.0 && dist > 0.0 {
                                    position + (p - position) * (boundary.distance / dist).min(1.0)
                                } else {
                                    position
                                };
                                if DRIVEWAYS
                                    && driveway
                                        .as_mut()
                                        .is_some_and(|guard| guard.blocks(position, next))
                                {
                                    return position;
                                }
                                if self
                                    .get_travers()
                                    .is_some_and(|&traversal| !move_on(traversal, position, next))
                                {
                                    return position;
                                }
                                self.observe_road_entry(position, next, map, &mut on_road_entry);
                                if reason == HoldReason::InvalidTraversal {
                                    if let ItineraryKind::Route(ref active, kind) = self.kind {
                                        *self = Self::retry_after_edit(kind, active.target, self.road_approach);
                                    }
                                }
                                admission.hold = Some(reason);
                                return next;
                            }
                        }
                    }
                }
            }
            if let Some(driveway) = driveway.as_mut().filter(|_| DRIVEWAYS) {
                let next = if dist <= (dist_to_move + snap).max(0.0) {
                    p
                } else {
                    position + (p - position) * (dist_to_move.max(0.0) / dist)
                };
                if driveway.blocks(position, next) {
                    return position;
                }
            }
            // Coincident waypoints still advance after a tolerance snap.
            if dist <= (dist_to_move + snap).max(0.0) {
                if self
                    .get_travers()
                    .is_some_and(|&traversal| !move_on(traversal, position, p))
                {
                    return position;
                }
                self.observe_road_entry(position, p, map, &mut on_road_entry);
                dist_to_move -= dist;
                position = p;
                if self.is_terminal() {
                    self.advance(map, position, on_road_entry.is_some());
                    return p;
                }

                if self.remaining_points() > 1 {
                    self.advance(map, position, on_road_entry.is_some());
                    continue;
                }

                let k = unwrap_or!(self.get_travers(), {
                    *self = Itinerary::NONE;
                    return p;
                });

                if k.can_pass(time, map.lanes()) {
                    if let Some(admission) = &mut admission {
                        if admission.observe_discharge {
                            if let TraverseKind::Turn(turn) = k.kind {
                                admission.discharged = Some(turn);
                            }
                        }
                    }
                    self.advance(map, position, on_road_entry.is_some());
                    on_initial_approach = false;
                    continue;
                }
                return p;
            }

            let next = position + (p - position).normalize_to(dist_to_move.max(0.0));
            if self
                .get_travers()
                .is_some_and(|&traversal| !move_on(traversal, position, next))
            {
                return position;
            }
            self.observe_road_entry(position, next, map, &mut on_road_entry);
            return next;
        }

        if let ItineraryKind::WaitForReroute {
            kind,
            target,
            ref mut wait_ticks,
        } = self.kind
        {
            if *wait_ticks > 0 {
                *wait_ticks -= 1;
                return position;
            }
            let mut next = unwrap_or!(route(position, target, kind), {
                *wait_ticks = REROUTE_WAIT_TICKS;
                return position;
            });
            if on_road_entry.is_some() {
                let previous = self
                    .road_approach
                    .last_road()
                    .filter(|&road| map.get(road).is_some());
                next.road_approach = next.initial_road_approach(map, position, previous);
            }
            *self = next;
        }

        position
    }

    pub fn random_route(
        rng: u64,
        position: Vec3,
        tick: Tick,
        map: &Map,
        pathkind: PathKind,
    ) -> Option<Itinerary> {
        let lanes = &map.lanes;
        let lane = lanes.values().nth(rng as usize % lanes.len())?;
        if !pathkind.authorized_lane(lane.kind) {
            return None;
        }
        Itinerary::route(
            tick,
            position,
            lane.points.point_along(lane.points.length() * 0.5),
            map,
            pathkind,
        )
    }

    pub fn end_pos(&self) -> Option<Vec3> {
        match self.kind {
            ItineraryKind::None => None,
            ItineraryKind::WaitUntil(_)
            | ItineraryKind::WaitForReroute { .. }
            | ItineraryKind::PedestrianEgress(_)
            | ItineraryKind::Driveway(_) => None,
            ItineraryKind::Simple(e) => Some(e),
            ItineraryKind::Route(ref r, _) => Some(r.end_pos),
        }
    }

    pub fn target(&self) -> Option<ItineraryTarget> {
        match &self.kind {
            ItineraryKind::Route(route, _) => Some(route.target),
            ItineraryKind::WaitForReroute { target, .. } => Some(*target),
            ItineraryKind::Simple(end) => Some(ItineraryTarget::Position(*end)),
            ItineraryKind::PedestrianEgress(egress) => egress.retry.map(|(target, _)| target),
            _ => None,
        }
    }

    pub fn remaining_points(&self) -> usize {
        self.reversed_local_path.len()
    }

    /// A stationary entity with a route may still cross an itinerary
    /// boundary. Only the inert itinerary kinds can skip movement.
    pub(crate) fn is_inert(&self) -> bool {
        self.reversed_local_path.is_empty()
            && matches!(
                self.kind,
                ItineraryKind::None | ItineraryKind::WaitUntil(_) | ItineraryKind::Driveway(_)
            )
    }

    pub(crate) fn at_driveway(phase: crate::transportation::DrivewayPhase) -> Self {
        Self {
            kind: ItineraryKind::Driveway(phase),
            ..Self::default()
        }
    }

    pub(crate) fn driveway_phase(&self) -> Option<crate::transportation::DrivewayPhase> {
        match self.kind {
            ItineraryKind::Driveway(phase) => Some(phase),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        match &self.kind {
            ItineraryKind::None | ItineraryKind::WaitUntil(_) | ItineraryKind::Driveway(_) => true,
            ItineraryKind::WaitForReroute { .. } | ItineraryKind::PedestrianEgress(_) => false,
            ItineraryKind::Simple(_) => self.remaining_points() <= 1,
            ItineraryKind::Route(Route { reversed_route, .. }, _) => {
                reversed_route.is_empty() && self.remaining_points() <= 1
            }
        }
    }

    pub fn get_point(&self) -> Option<Vec3> {
        self.reversed_local_path.last().copied()
    }

    pub fn get_terminal(&self) -> Option<Vec3> {
        if !self.is_terminal() {
            return None;
        }
        match self.kind {
            ItineraryKind::None
            | ItineraryKind::WaitUntil(_)
            | ItineraryKind::WaitForReroute { .. }
            | ItineraryKind::PedestrianEgress(_)
            | ItineraryKind::Driveway(_) => None,
            ItineraryKind::Simple(e) => Some(e),
            ItineraryKind::Route(Route { end_pos, .. }, _) => Some(end_pos),
        }
    }

    pub fn get_travers(&self) -> Option<&Traversable> {
        match &self.kind {
            ItineraryKind::None
            | ItineraryKind::WaitUntil(_)
            | ItineraryKind::Simple(_)
            | ItineraryKind::WaitForReroute { .. }
            | ItineraryKind::Driveway(_) => None,
            ItineraryKind::Route(Route { cur, .. }, _) => Some(cur),
            ItineraryKind::PedestrianEgress(egress) => egress.crossing.as_ref(),
        }
    }

    pub fn get_route(&self) -> Option<&Route> {
        match &self.kind {
            ItineraryKind::Route(r, _) => Some(r),
            _ => None,
        }
    }

    pub fn local_path(&self) -> &[Vec3] {
        &self.reversed_local_path
    }

    pub fn prepend_local_path(&mut self, points: impl IntoIterator<Item = Vec3>) {
        self.reversed_local_path.extend(points);
    }

    pub fn has_ended(&self, time: f64) -> bool {
        match self.kind {
            ItineraryKind::WaitUntil(x) => time > x,
            ItineraryKind::WaitForReroute { .. } | ItineraryKind::PedestrianEgress(_) => false,
            ItineraryKind::Route(
                Route {
                    ref reversed_route, ..
                },
                _,
            ) => reversed_route.is_empty() && self.reversed_local_path.is_empty(),
            _ => self.reversed_local_path.is_empty(),
        }
    }

    pub(crate) fn is_missing(&self) -> bool {
        matches!(self.kind, ItineraryKind::None)
    }

    pub fn is_none_or_wait(&self) -> bool {
        matches!(self.kind, ItineraryKind::None | ItineraryKind::WaitUntil(_))
    }

    pub fn is_simple(&self) -> bool {
        matches!(self.kind, ItineraryKind::Simple(_))
    }

    pub(crate) fn is_pedestrian_egress(&self) -> bool {
        matches!(self.kind, ItineraryKind::PedestrianEgress(_))
    }

    fn requires_pedestrian_egress(&self, has_route_step: bool) -> bool {
        match self.kind {
            ItineraryKind::None => !has_route_step,
            ItineraryKind::WaitForReroute {
                kind: PathKind::Pedestrian,
                wait_ticks,
                ..
            } => wait_ticks != 0 || !has_route_step,
            ItineraryKind::PedestrianEgress(_) => true,
            _ => false,
        }
    }

    fn needs_clearance_check(&self, tick: u64, has_route_step: bool) -> bool {
        self.requires_pedestrian_egress(has_route_step)
            && self.get_point().is_none()
            && match self.kind {
                ItineraryKind::PedestrianEgress(egress) => tick >= egress.check_at.0,
                _ => true,
            }
    }

    fn prepare_egress(&mut self, map: &Map, position: Vec3, tick: u64) -> bool {
        let retry = match self.kind {
            ItineraryKind::WaitForReroute { kind: PathKind::Pedestrian, target, wait_ticks } => {
                Some((target, wait_ticks))
            }
            ItineraryKind::PedestrianEgress(egress) => egress.retry,
            _ => None,
        };
        let (crossing, reversed_local_path) = match pedestrian_clearance(map, position) {
            PedestrianClearance::Clear => {
                if self.is_pedestrian_egress() {
                    self.finish_egress(retry);
                }
                return true;
            }
            PedestrianClearance::Hold => (None, vec![]),
            PedestrianClearance::Crosswalk { traversal, projection, sidewalk } => {
                let mut points = vec![sidewalk];
                if position != projection {
                    points.push(projection);
                }
                (Some(traversal), points)
            }
            PedestrianClearance::Curb { sidewalk, exit } => (Some(sidewalk), vec![exit]),
        };
        self.reversed_local_path = reversed_local_path;
        self.kind = ItineraryKind::PedestrianEgress(PedestrianEgress {
            crossing,
            retry,
            check_at: Tick(tick.saturating_add(EGRESS_RECHECK_TICKS)),
        });
        false
    }

    fn advance_egress<const DRIVEWAYS: bool>(
        &mut self,
        map: &Map,
        mut position: Vec3,
        mut distance: f32,
        mut driveway: Option<&mut DrivewayMovement<'_>>,
    ) -> Vec3 {
        let ItineraryKind::PedestrianEgress(ref mut egress) = self.kind else {
            unreachable!();
        };
        let Some(crossing) = egress.crossing else {
            return position;
        };
        if !exit_path_still_valid(map, crossing, position, &self.reversed_local_path) {
            self.reversed_local_path.clear();
            egress.crossing = None;
            egress.check_at = Tick(0);
            return position;
        }
        while let Some(&next) = self.reversed_local_path.last() {
            let segment = next - position;
            let length = segment.mag();
            let reached = if length > distance {
                position + segment * (distance / length)
            } else {
                next
            };
            if DRIVEWAYS
                && driveway
                    .as_mut()
                    .is_some_and(|guard| guard.blocks(position, reached))
            {
                return position;
            }
            if length > distance {
                return reached;
            }
            self.reversed_local_path.pop();
            position = next;
            distance -= length;
        }
        let retry = egress.retry;
        self.finish_egress(retry);
        position
    }

    fn finish_egress(&mut self, retry: Option<(ItineraryTarget, u16)>) {
        if let Some((target, wait_ticks)) = retry {
            self.kind = ItineraryKind::WaitForReroute {
                kind: PathKind::Pedestrian,
                target,
                wait_ticks,
            };
        } else {
            *self = Itinerary::NONE;
        }
    }
}

fn driving_lane_points(map: &Map, lane: LaneID, offset: f32) -> Option<&PolyLine3> {
    directed_lane_points(map, lane, offset, PathKind::Vehicle)
}

fn directed_lane_points(
    map: &Map,
    lane: LaneID,
    offset: f32,
    kind: PathKind,
) -> Option<&PolyLine3> {
    let lane = map.get(lane)?;
    (kind.authorized_lane(lane.kind) && (0.0..=lane.points.length()).contains(&offset))
        .then_some(&lane.points)
}

fn directed_local_path(
    points: &PolyLine3,
    start: Vec3,
    offset: f32,
    end: Vec3,
) -> Option<Vec<Vec3>> {
    let (start, start_segment) = points.project_segment(start);
    let segment_start = start_segment.checked_sub(1)?;
    let mut distance = points
        .segments()
        .take(segment_start)
        .map(|segment| segment.src.distance(segment.dst))
        .sum::<f32>();
    if offset + 0.0001 < distance + points[segment_start].distance(start) {
        return None;
    }
    let mut path = vec![start];
    for segment in points.segments().skip(segment_start) {
        distance += segment.src.distance(segment.dst);
        if distance >= offset {
            break;
        }
        path.push(segment.dst);
    }
    path.push(end);
    Some(path)
}

impl Inspect<ItineraryKind> for ItineraryKind {
    fn render(d: &ItineraryKind, label: &'static str, ui: &mut Ui, args: &InspectArgs) {
        match *d {
            ItineraryKind::None => {
                ui.label(format!("None {label}"));
            }
            ItineraryKind::WaitUntil(time) => {
                ui.label(format!("WaitUntil({time}) {label}"));
            }
            ItineraryKind::Simple(e) => {
                ui.label(format!("Simple {label} to {e}"));
            }
            ItineraryKind::Route(ref r, _) => {
                <Route as Inspect<Route>>::render(r, label, ui, args);
            }
            ItineraryKind::WaitForReroute { wait_ticks, .. } => {
                ui.label(format!("wait for reroute: {wait_ticks}"));
            }
            ItineraryKind::Driveway(phase) => {
                ui.label(format!("{phase:?}"));
            }
            ItineraryKind::PedestrianEgress(egress) => {
                ui.label(if egress.crossing.is_some() { "Clearing crosswalk" } else { "Waiting for a safe exit" });
            }
        };
    }

    fn render_mut(
        d: &mut ItineraryKind,
        label: &'static str,
        ui: &mut Ui,
        args: &InspectArgs,
    ) -> bool {
        match *d {
            ItineraryKind::None => {
                ui.label(format!("None {label}"));
            }
            ItineraryKind::WaitUntil(time) => {
                ui.label(format!("WaitUntil({time}) {label}"));
            }
            ItineraryKind::Simple(e) => {
                ui.label(format!("Simple {label} to {e}"));
            }
            ItineraryKind::Route(ref mut r, _) => {
                return <Route as Inspect<Route>>::render_mut(r, label, ui, args);
            }
            ItineraryKind::WaitForReroute { wait_ticks, .. } => {
                ui.label(format!("wait for reroute: {wait_ticks}"));
            }
            ItineraryKind::PedestrianEgress(_) | ItineraryKind::Driveway(_) => {
                Self::render(d, label, ui, args)
            }
        };
        false
    }
}

fn movement_reaches_lane(lane: &Lane, from: Vec3, to: Vec3) -> bool {
    let half_width = lane.kind.width() * 0.5;
    let from_dist2 = lane.points.project_dist2(from);
    if from_dist2 <= half_width * half_width {
        return true;
    }
    // Every point of the movement is within its length of the start. A start
    // farther than that from the lane cannot reach it. The margin covers rounding.
    let reach = half_width + from.distance(to) + 1.0;
    if from_dist2 > reach * reach {
        return false;
    }
    if within_lane(lane, to) {
        return true;
    }
    if from == to {
        return false;
    }
    let movement = Segment3::new(from, to);
    let along_movement = movement.vec();
    let radius2 = (lane.kind.width() * 0.5).powi(2);
    lane.points.segments().any(|segment| {
        if movement.project(segment.src).distance2(segment.src) <= radius2
            || movement.project(segment.dst).distance2(segment.dst) <= radius2
        {
            return true;
        }

        // The endpoints are outside the lane. Check the closest points in
        // the two segment interiors for a movement that crosses the lane.
        let along_lane = segment.vec();
        let normal = along_movement.cross(along_lane);
        let normal_sq = normal.mag2();
        let offset = segment.src - from;
        if normal_sq == 0.0 || offset.dot(normal).powi(2) > radius2 * normal_sq {
            return false;
        }
        let movement_dist = offset.cross(along_lane).dot(normal);
        let lane_dist = offset.cross(along_movement).dot(normal);
        let range = 0.0..=normal_sq;
        range.contains(&movement_dist) && range.contains(&lane_dist)
    })
}

struct DrivewayMovement<'a> {
    traffic: &'a crate::transportation::DrivewayTraffic,
    actor: Option<crate::VehicleID>,
    half_extent: f32,
    held: &'a mut bool,
}

impl DrivewayMovement<'_> {
    fn blocks(&mut self, from: Vec3, to: Vec3) -> bool {
        let blocked = self
            .traffic
            .blocks_step(self.actor, self.half_extent, from, to);
        *self.held |= blocked;
        blocked
    }
}

struct MovementAdmission<'a> {
    frame: &'a mut JunctionFrame,
    state: &'a mut JunctionAdmission,
    user: MovingOwner,
    hold: Option<HoldReason>,
    observe_discharge: bool,
    discharged: Option<crate::map::TurnID>,
}

pub fn itinerary_update(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::itinerary_update");
    let driveways = resources.read::<crate::transportation::DrivewayTraffic>();
    if driveways.has_crossing() {
        update_itineraries::<true>(world, resources, &driveways);
    } else {
        update_itineraries::<false>(world, resources, &driveways);
    }
}

fn update_itineraries<const DRIVEWAYS: bool>(
    world: &mut World,
    resources: &Resources,
    driveways: &crate::transportation::DrivewayTraffic,
) {
    let time = &*resources.read::<GameTime>();
    let map = &*resources.read::<Map>();
    let failures = &mut resources.write::<ParkingManagement>().reroute_failures;
    failures.sync_map(map);
    let mut throughput = resources.write::<RoadThroughput>();
    let tick = elapsed_ticks(resources).0;
    throughput.begin_tick(map, tick);
    let mut remaining_egress_checks = MAX_EGRESS_CHECKS_PER_TICK;
    let mut checked_clear = Vec::new();
    if map.has_junction_rules() {
        world.humans.edit_active(HumanWork::Itinerary, |id, human| {
            if remaining_egress_checks != 0
                && human.location == Location::Outside
                && human.collider.is_some()
                && human
                    .it
                    .needs_clearance_check(tick, human.router.has_route_step())
                && (human.it.is_pedestrian_egress() || needs_local_recovery(map, human.trans.pos))
            {
                remaining_egress_checks -= 1;
                human.speed.0 = 0.0;
                let position = human.trans.pos;
                if human.it.prepare_egress(map, position, tick) {
                    checked_clear.push(id);
                }
            }
        });
    }
    // Boarding and routing have settled. Build grants from one frozen view
    // before any user's final itinerary movement.
    let mut junction_state = resources.write::<JunctionAdmission>();
    let mut junction_frame = if map.has_junction_controller() {
        let mut grid = resources.write::<TransportGrid>();
        grid.refresh_admission_colliders(world);
        JunctionFrame::prepare(world, map, &grid, &mut junction_state, tick)
    } else {
        junction_state.clear();
        JunctionFrame::default()
    };
    let mut diagnostics = resources.write::<QueueDiagnostics>();
    let observing = diagnostics.observing();

    use crate::transportation::TraversalOwner;

    // Observe transitions during the ordinary movement pass. Indoor idle
    // humans are already excluded by the itinerary activity set.
    let mut observed = resources
        .write::<TransportGrid>()
        .begin_traversal_update(map, tick);

    let mut freight = resources.write::<crate::economy::cargo::FreightTrace>();
    let mut update = |it: &mut Itinerary,
                      pos: &mut Vec3,
                      speed: f32,
                      kind: Option<RoadUserKind>,
                      user: Option<MovingOwner>| {
        if it.is_inert() {
            return (None, None, None);
        }
        let truck = match (kind, user) {
            (Some(RoadUserKind::Truck), Some(MovingOwner { owner: EntryOwner::Vehicle(id), .. })) => {
                Some(id)
            }
            _ => None,
        };
        let record = kind.map(|kind| {
            let throughput = &mut *throughput;
            move |road, path| {
                let expected = match kind {
                    RoadUserKind::Pedestrian => PathKind::Pedestrian,
                    _ => PathKind::Vehicle,
                };
                if path == expected {
                    throughput.record_entry(road, kind, tick);
                }
            }
        });
        let mut admission = user.map(|user| MovementAdmission {
            frame: &mut junction_frame,
            state: &mut junction_state,
            user,
            hold: None,
            observe_discharge: observing && matches!(user.owner, EntryOwner::Vehicle(_)),
            discharged: None,
        });
        let mut driveway_held = false;
        let driveway = user.filter(|_| DRIVEWAYS).map(|user| DrivewayMovement {
            traffic: driveways,
            actor: match user.owner {
                EntryOwner::Vehicle(id) => Some(id),
                EntryOwner::Human(_) => None,
            },
            half_extent: user.half_extent,
            held: &mut driveway_held,
        });
        *pos = it.update_guarded::<DRIVEWAYS>(
            *pos,
            speed * DELTA,
            time.seconds,
            map,
            |start, target, kind| failures.route(time.tick, start, target, map, kind),
            record,
            admission.as_mut(),
            |traversal, from, to| {
                if let Some(truck) = truck {
                    freight.observe_truck_motion(truck, traversal.kind, from, to);
                }
                true
            },
            driveway,
        );
        if DRIVEWAYS && driveway_held {
            return (Some(HoldReason::Conflict), None, None);
        }
        admission.map_or((None, None, None), |guard| {
            let observed_hold = match guard.user.owner {
                EntryOwner::Vehicle(id) if observing => guard.frame.denial(id),
                _ => None,
            };
            (guard.hold, guard.discharged, observed_hold)
        })
    };
    world.humans.edit_active(HumanWork::Itinerary, |id, human| {
        let outside = human.location == Location::Outside;
        let has_route_step = human.router.has_route_step();
        let position = human.trans.pos;
        let mut checked = checked_clear.contains(&id);
        if outside
            && human.collider.is_some()
            && human.it.needs_clearance_check(tick, has_route_step)
            && !checked
            && (human.it.is_pedestrian_egress() || needs_local_recovery(map, position))
        {
            human.speed.0 = 0.0;
            if remaining_egress_checks == 0 {
                return;
            }
            remaining_egress_checks -= 1;
            checked = human.it.prepare_egress(map, position, tick);
        }
        let clearing = human.it.is_pedestrian_egress();
        let kind = outside.then_some(RoadUserKind::Pedestrian);
        let speed = human.speed.0;
        let (itinerary, pos) = human.itinerary_mut();
        let (held, _, _) = update(
            itinerary,
            pos,
            speed,
            kind,
            outside.then_some(MovingOwner { owner: EntryOwner::Human(id), half_extent: 0.3 }),
        );
        if !outside {
            return;
        }
        if let Some(index) = &mut observed {
            index.record(map, TraversalOwner::Human(id), itinerary);
        }
        if held.is_some() {
            human.speed.0 = 0.0;
        }
        match (human.it.get_point(), human.collider) {
            (None, Some(collider)) if !human.it.is_pedestrian_egress() => {
                // Movement can consume the last point on this tick. Check the
                // final pose before releasing an obstruction at a ruled junction.
                if !clearing
                    && (!checked || position != human.trans.pos)
                    && needs_local_recovery(map, human.trans.pos)
                {
                    human.speed.0 = 0.0;
                    if remaining_egress_checks == 0 {
                        return;
                    }
                    remaining_egress_checks -= 1;
                    let position = human.trans.pos;
                    if !human.it.prepare_egress(map, position, tick) {
                        return;
                    }
                }
                resources.write::<TransportGrid>().remove_maintain(collider.0);
                human.collider = None;
                human.speed.0 = 0.0;
            }
            (Some(_), None) => {
                let pos = human.trans.pos;
                let mut grid = resources.write::<TransportGrid>();
                let collider = put_pedestrian_in_transport_grid(&mut grid, pos);
                grid.get_mut(collider.0).unwrap().1.height = pos.z;
                human.collider = Some(collider);
            }
            _ => {}
        }
    });
    for (id, vehicle) in world.vehicles.iter_mut() {
        let kind = match vehicle.vehicle.kind {
            VehicleKind::Car => RoadUserKind::Car,
            VehicleKind::Truck => RoadUserKind::Truck,
            VehicleKind::Bus => RoadUserKind::Bus,
        };
        let before = observing.then(|| vehicle.it.get_travers().copied()).flatten();
        let (held, discharged, observed_hold) = update(
            &mut vehicle.it,
            &mut vehicle.trans.pos,
            vehicle.speed.0,
            Some(kind),
            Some(MovingOwner {
                owner: EntryOwner::Vehicle(id),
                half_extent: vehicle.vehicle.kind.width() * 0.5,
            }),
        );
        if held.is_some() {
            vehicle.speed.0 = 0.0;
        }
        if observing {
            if let Some(reason) = held.or(observed_hold) {
                let reason = match reason {
                    HoldReason::Conflict => QueueHoldReason::Conflict(None),
                    HoldReason::ExitFull => QueueHoldReason::ExitFull,
                    HoldReason::StopDwell => QueueHoldReason::StopDwell,
                    HoldReason::InvalidTraversal => QueueHoldReason::InvalidTraversal,
                };
                diagnostics.record_hold(id, vehicle.it.get_travers().copied().or(before), tick, reason);
            }
        }
        if let Some(turn) = discharged {
            diagnostics.record_discharge(turn, tick);
        }
        if let Some(index) = &mut observed {
            index.record(map, TraversalOwner::Vehicle(id), &vehicle.it);
        }
    }

    let reservations = resources.read::<TrainReservations>();
    let lengths = world
        .trains
        .iter()
        .map(|(id, train)| (id, train.locomotive.length - 10.0))
        .collect();
    for (id, train) in world.trains.iter_mut() {
        if !freight.may_move(id) {
            train.speed.0 = 0.0;
        } else if train.speed.0 > 0.0 && !train.it.is_inert() {
            train.trans.pos = train.it.update_guarded::<false>(
                train.trans.pos,
                train.speed.0 * DELTA,
                time.seconds,
                map,
                |_, _, _| None,
                None::<fn(RoadID, PathKind)>,
                None,
                |traversal, from, to| {
                    let Some(track) = traversal.raw_points(map) else {
                        return false;
                    };
                    if !can_move_train(
                        &reservations,
                        &lengths,
                        id,
                        traversal.kind,
                        track,
                        &train.res,
                        to,
                    ) {
                        return false;
                    }
                    if from != to {
                        record_train_motion(
                            &mut train.res,
                            train.locomotive.length,
                            map,
                            traversal,
                            from,
                            to,
                        );
                        train.leader.past.push(to);
                        freight.observe_motion(id, traversal.kind, from, to, tick);
                    }
                    true
                },
                None,
            );
        }
        if let Some(index) = &mut observed {
            index.record(map, TraversalOwner::Train(id), &train.it);
        }
    }
    if let Some(snapshot) = observed {
        resources
            .write::<TransportGrid>()
            .publish_traversal_users(snapshot);
    }

    world.wagons.values_mut().for_each(|wagon| {
        let leader = &unwrap_ret!(world.trains.get(wagon.itfollower.leader)).leader;
        let (pos, dir) = wagon.itfollower.head.update(&leader.past);
        let (pos2, dir2) = wagon.itfollower.tail.update(&leader.past);
        wagon.trans.pos = (pos + pos2) * 0.5;
        wagon.trans.dir = (dir + dir2).try_normalize().unwrap_or(dir);
    });
}

#[cfg(test)]
use crate::map::LaneKind;
#[cfg(test)]
#[path = "../tests/p7_junction.rs"]
mod p7_junction;

#[cfg(test)]
thread_local! {
    static ROUTE_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{LaneKind, LanePatternBuilder, ProjectFilter, TurnKind};
    use crate::souls::human::spawn_human;
    use crate::tests::TestCtx;
    use crate::transportation::{spawn_truck_at_depot, Location};
    use crate::Simulation;
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;

    fn update_at(sim: &mut Simulation, tick: u64) {
        *sim.write::<GameTime>() = GameTime::new(Tick(tick));
        itinerary_update(&mut sim.world, &mut sim.resources);
    }

    #[test]
    fn itinerary_consumes_duplicate_waypoints_after_tolerance_snap() {
        let movement = 0.024_f32;
        let seam = Vec3::X * (movement + 0.01);
        let mut route = Itinerary::simple(vec![seam, seam, Vec3::X]);
        let map = Map::empty();

        let position = route.update(Vec3::ZERO, movement, Tick(0), 0, &map);
        assert_eq!(position, seam);
        assert_eq!(route.get_point(), Some(Vec3::X));
        let next = route.update(position, movement, Tick(1), 0, &map);
        assert!((next.x - (seam.x + movement)).abs() < 1e-6);
    }

    fn apply_stop_rule(sim: &mut Simulation, junction: crate::map::IntersectionID, road: RoadID) {
        use crate::map::road_edit::{junction_edit_stamp, preview_junction_edit};
        use crate::map::{ApproachRole, JunctionRules, LightPolicy};
        use crate::world_command::{CommandOutcomes, PrepareBudget, WorldCommand};

        sim.map_mut()
            .update_intersection(junction, |inter| inter.light_policy = LightPolicy::NoLights);
        let rules = JunctionRules::new(vec![(road, ApproachRole::Stop)], vec![]).unwrap();
        let budget = &mut PrepareBudget::for_preview(100_000);
        let expected = junction_edit_stamp(sim, junction, budget).unwrap();
        let preview = preview_junction_edit(sim, junction, expected, &rules, budget).unwrap();
        sim.apply_commands([&WorldCommand::MapSetJunctionRules {
            junction,
            expected,
            rules,
            quote: preview.quote,
        }]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    }

    #[test]
    fn p7_life_clipped_crosswalk_keeps_collider_at_quoted_junction() {
        use crate::map::{MapProject, ProjectKind};

        let mut ctx = TestCtx::new();
        let center = vec3(420.0, 420.0, 10.0);
        let pattern = LanePatternBuilder::new().n_lanes(3).build();
        let (junction, road, position, ends) = {
            let mut map = ctx.g.map_mut();
            let (_, road) = map
                .make_connection(
                    MapProject::ground(center),
                    MapProject::ground(center + vec3(275.0, 0.0, 0.0)),
                    None,
                    &pattern,
                )
                .unwrap();
            let junction = map.roads()[road].src;
            for angle in [30.0_f32, 60.0, 180.0, 270.0] {
                let angle = angle.to_radians();
                let (sin, cos) = angle.sin_cos();
                map.make_connection(
                    MapProject {
                        kind: ProjectKind::Intersection(junction),
                        pos: center,
                    },
                    MapProject::ground(center + vec3(275.0 * cos, 275.0 * sin, 0.0)),
                    None,
                    &pattern,
                )
                .unwrap();
            }
            map.finalize_import(&[], &[]);
            let crosswalk = map
                .get(junction)
                .unwrap()
                .crosswalks()
                .find(|turn| {
                    !map.spatial_map()
                        .query_around(turn.points.middle().xy(), 1.25, ProjectFilter::INTER)
                        .any(|candidate| candidate == ProjectKind::Intersection(junction))
                })
                .expect("a clipped crosswalk beyond the junction circle");
            (
                junction,
                road,
                crosswalk.points.middle(),
                [crosswalk.points.first(), crosswalk.points.last()],
            )
        };
        apply_stop_rule(&mut ctx.g, junction, road);

        let house = ctx.build_house_near(vec3(610.0, 420.0, 10.0).xy());
        let id = spawn_human(&mut ctx.g, house).unwrap();
        let collider =
            put_pedestrian_in_transport_grid(&mut ctx.g.write::<TransportGrid>(), position);
        ctx.g.world.edit_human(id, |human| {
            human.location = Location::Outside;
            human.trans.pos = position;
            human.collider = Some(collider);
            human.it = Itinerary::NONE;
        });
        let tick = ctx.g.get_tick();
        update_at(&mut ctx.g, tick + 1);
        assert_eq!(ctx.g.world.humans[id].collider, Some(collider));

        let mut schedule = Simulation::schedule();
        schedule.retain_systems(|name| {
            matches!(name, "pedestrian_decision_system" | "itinerary_update")
        });
        for _ in 0..1600 {
            ctx.g.tick(&mut schedule, &[]);
        }
        let human = &ctx.g.world.humans[id];
        assert!(ends.iter().any(|&end| human.trans.pos.is_close(end, 0.1)));
        assert!(human.collider.is_none());
    }

    #[test]
    fn p7_life_alpska_demolished_destination_physically_clears_crosswalk() {
        use crate::map_dynamic::{Destination, RoutingStep};
        use crate::souls::human::HumanDecisionKind;
        use crate::transportation::spawn_driving_vehicle;
        use crate::world_command::{CommandOutcomes, WorldCommand};
        use geom::Transform;

        let mut ctx = TestCtx::new();
        let junction = vec3(270.0, 180.0, 0.0);
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), junction, vec3(470.0, 180.0, 0.0)]);
        ctx.build_roads(&[junction, vec3(270.0, 350.0, 0.0)]);
        let house = ctx.build_house_near(vec3(160.0, 180.0, 0.0).xy());
        let target_house = ctx.build_house_near(vec3(420.0, 180.0, 0.0).xy());
        let door = ctx.g.map().get(target_house).unwrap().door_pos;
        let id = spawn_human(&mut ctx.g, house).unwrap();
        let (ruled_junction, approach) = {
            let map = ctx.g.map();
            let inter = map
                .intersections()
                .values()
                .find(|inter| inter.pos.xy() == junction.xy())
                .unwrap();
            (inter.id, inter.roads[0])
        };
        apply_stop_rule(&mut ctx.g, ruled_junction, approach);
        let (position, destination, car_start, car_end, car_route) = {
            let map = ctx.g.map();
            let junction = map
                .intersections()
                .values()
                .find(|intersection| intersection.pos.xy() == junction.xy())
                .unwrap();
            let crosswalk = junction
                .turns()
                .find(|turn| turn.kind == TurnKind::Crosswalk)
                .unwrap();
            let crossing_road = map.get(crosswalk.id.src).unwrap().parent;
            let turn = junction
                .turns()
                .filter(|turn| turn.kind == TurnKind::Driving)
                .find(|turn| map.get(turn.id.src).unwrap().parent == crossing_road)
                .unwrap();
            let lane = map.get(turn.id.src).unwrap();
            let approach = lane.points.length() - 8.0;
            let (pos, dir) = lane.points.point_dir_along(approach);
            let route = Itinerary::route_between_lanes(
                Tick(ctx.g.get_tick()),
                &map,
                (turn.id.src, approach),
                (turn.id.dst, 30.0),
            )
            .unwrap();
            (
                crosswalk.points.project(lane.points.last()),
                crosswalk.points.last(),
                Transform::new_dir(pos, dir),
                route.end_pos().unwrap(),
                route,
            )
        };
        let original_car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, car_start).unwrap();
        ctx.g.world.vehicles[original_car].it = car_route;
        let collider = put_pedestrian_in_transport_grid(&mut ctx.g.write::<TransportGrid>(), position);
        ctx.g.write::<TransportGrid>().get_mut(collider.0).unwrap().1.height = position.z;
        let tick = ctx.g.get_tick();
        let target = Destination::Building(target_house);
        ctx.g.world.edit_human(id, |human| {
            human.trans.pos = position;
            human.location = Location::Outside;
            human.decision.kind = HumanDecisionKind::GoTo(target);
            human.router.set_test_route(
                target,
                vec![RoutingStep::WalkTo(door), RoutingStep::GetInBuilding(target_house)],
            );
            human.it = Itinerary::simple(vec![destination, door]);
            human.collider = Some(collider);
            human.speed.0 = 1.4;
        });
        let human = &ctx.g.world.humans[id];
        assert_eq!(human.router.target_dest, Some(target));
        assert!(matches!(human.router.cur_step(), Some(RoutingStep::WalkTo(point)) if *point == door));
        assert!(human.it.get_point().is_some());

        ctx.g.apply_commands([&WorldCommand::MapRemoveBuilding(target_house)]);
        assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);
        let human = &ctx.g.world.humans[id];
        assert_eq!(human.location, Location::Outside);
        assert!(matches!(human.decision.kind, HumanDecisionKind::Yield));
        assert!(human.router.target_dest.is_none());
        assert!(!human.router.has_route_step());
        assert!(human.it.is_missing());
        assert_eq!(human.collider, Some(collider));

        update_at(&mut ctx.g, tick + 1);
        assert!(ctx.g.world.humans[id].collider.is_some());
        assert!(ctx.g.read::<TransportGrid>().get(collider.0).is_some());
        assert_eq!(ctx.g.world.humans[id].trans.pos, position);
        assert_eq!(ctx.g.world.humans[id].speed.0, 0.0);

        let mut schedule = Simulation::schedule();
        schedule.retain_systems(|name| {
            matches!(
                name,
                "pedestrian_decision_system"
                    | "transport_grid_synchronize"
                    | "vehicle_decision_system"
                    | "itinerary_update"
            )
        });
        for _ in 0..1600 {
            ctx.g.tick(&mut schedule, &[]);
        }
        let human = &ctx.g.world.humans[id];
        assert!(human.trans.pos.distance(position) > 1.0);
        assert!(human.collider.is_none());
        assert!(ctx.g.read::<TransportGrid>().get(collider.0).is_none());
        let car = &ctx.g.world.vehicles[original_car];
        assert!(car.it.has_ended(0.0));
        assert!(car.trans.pos.distance(car_end) < 0.1);

        ctx.g.world.edit_human(id, |human| {
            human.router.go_to(Destination::Outside(destination));
        });
        let (world, resources) = ctx.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        crate::map_dynamic::routing_update_system(world, resources);
        update_at(&mut ctx.g, tick + 1602);
        let human = &ctx.g.world.humans[id];
        assert!(human.it.get_point().is_some());
        assert!(ctx
            .g
            .read::<TransportGrid>()
            .get(human.collider.unwrap().0)
            .is_some());
    }

    #[test]
    fn p7_life_hraska_router_retries_disconnected_walk_with_its_original_target() {
        use crate::map_dynamic::{Destination, Router, RoutingStep};
        use crate::transportation::spawn_driving_vehicle;
        use crate::world_command::{CommandOutcomes, WorldCommand};
        use geom::Transform;

        let mut ctx = TestCtx::new();
        let junction = vec3(270.0, 180.0, 0.0);
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), junction, vec3(470.0, 180.0, 0.0)]);
        let approach = vec3(380.0, 350.0, 0.0);
        let far = vec3(380.0, 500.0, 0.0);
        ctx.build_roads(&[junction, approach, far, vec3(545.0, 500.0, 0.0)]);
        let house = ctx.build_house_near(vec3(160.0, 180.0, 0.0).xy());
        let id = spawn_human(&mut ctx.g, house).unwrap();
        let (position, destination, car_start, car_route, connector) = {
            let map = ctx.g.map();
            let crosswalk = map
                .intersections()
                .values()
                .flat_map(|intersection| intersection.turns())
                .filter(|turn| turn.kind == TurnKind::Crosswalk)
                .max_by_key(|turn| turn.id)
                .unwrap();
            let junction = map.get(crosswalk.id.parent).unwrap();
            let crossing_road = map.get(crosswalk.id.src).unwrap().parent;
            let turn = junction
                .turns()
                .filter(|turn| turn.kind == TurnKind::Driving)
                .find(|turn| map.get(turn.id.src).unwrap().parent == crossing_road)
                .unwrap();
            let lane = map.get(turn.id.src).unwrap();
            let approach_offset = lane.points.length() - 8.0;
            let (pos, dir) = lane.points.point_dir_along(approach_offset);
            let car_route = Itinerary::route_between_lanes(
                Tick(ctx.g.get_tick()),
                &map,
                (turn.id.src, approach_offset),
                (turn.id.dst, 30.0),
            )
            .unwrap();
            let road_at = |point: Vec3| {
                map.roads()
                    .values()
                    .find(|road| road.points().middle().xy().is_close(point.xy(), 1.0))
                    .unwrap()
            };
            let connector = road_at((approach + far) * 0.5).id;
            let far_road = road_at(vec3(462.5, 500.0, 0.0));
            let sidewalk = far_road
                .lanes_iter()
                .find(|(_, kind)| *kind == LaneKind::Walking)
                .unwrap()
                .0;
            (
                crosswalk.points.middle(),
                map.get(sidewalk).unwrap().points.middle(),
                Transform::new_dir(pos, dir),
                car_route,
                connector,
            )
        };
        let car_goal = car_route.end_pos().unwrap();
        let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, car_start).unwrap();
        ctx.g.world.vehicles[car].it = car_route;
        let collider = put_pedestrian_in_transport_grid(&mut ctx.g.write::<TransportGrid>(), position);
        ctx.g.write::<TransportGrid>().get_mut(collider.0).unwrap().1.height = position.z;
        ctx.g.world.edit_human(id, |human| {
            human.location = Location::Outside;
            human.trans.pos = position;
            human.collider = Some(collider);
            human.router = Router::new(None);
            human.router.go_to(Destination::Outside(destination));
        });

        let mut schedule = Simulation::schedule();
        schedule.retain_systems(|name| {
            matches!(
                name,
                "routing_changed_system"
                    | "routing_update_system"
                    | "pedestrian_decision_system"
                    | "transport_grid_synchronize"
                    | "vehicle_decision_system"
                    | "itinerary_update"
            )
        });
        ctx.g.tick(&mut schedule, &[]);
        let human = &ctx.g.world.humans[id];
        assert!(human.it.get_route().is_some());
        assert!(matches!(human.router.cur_step(), Some(RoutingStep::WalkTo(point)) if *point == destination));

        ctx.g.tick(&mut schedule, [&WorldCommand::MapRemoveRoad(connector)]);
        assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);
        for _ in 0..12_000 {
            let human = &ctx.g.world.humans[id];
            if human.it.is_wait_for_reroute().is_some() || human.it.is_pedestrian_egress() {
                break;
            }
            ctx.g.tick(&mut schedule, &[]);
        }
        let human = &ctx.g.world.humans[id];
        assert!(human.it.is_wait_for_reroute().is_some() || human.it.is_pedestrian_egress());
        assert_eq!(human.router.target_dest, Some(Destination::Outside(destination)));
        assert_eq!(human.it.target(), Some(ItineraryTarget::Position(destination)));

        let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
        for sim in [&mut ctx.g, &mut resumed] {
            let connect = {
                let map = sim.map();
                WorldCommand::MapMakeConnection {
                    from: map.project(approach, 0.0, ProjectFilter::ALL),
                    to: map.project(far, 0.0, ProjectFilter::ALL),
                    inter: None,
                    pat: LanePatternBuilder::default().build(),
                }
            };
            sim.apply_commands([&connect]);
            assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
        }
        for _ in 0..24000 {
            ctx.g.tick(&mut schedule, &[]);
        }
        let mut resumed_schedule = schedule;
        for _ in 0..24000 {
            resumed.tick(&mut resumed_schedule, &[]);
        }
        let human = &ctx.g.world.humans[id];
        assert!(human.trans.pos.distance(destination) < 0.1);
        assert!(human.it.has_ended(0.0));
        assert_eq!(resumed.world.humans[id].trans.pos, human.trans.pos);
        assert_eq!(resumed.world.humans[id].it.target(), human.it.target());
        let vehicle = &ctx.g.world.vehicles[car];
        assert!(vehicle.it.has_ended(0.0) && vehicle.trans.pos.distance(car_goal) < 0.1);
        assert_eq!(resumed.world.vehicles[car].trans.pos, vehicle.trans.pos);
    }

    #[test]
    fn p7_life_lesce_stalled_walk_clears_its_curb_and_queue() {
        use crate::transportation::spawn_driving_vehicle;
        use geom::Transform;

        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), vec3(470.0, 180.0, 0.0)]);
        let house = ctx.build_house_near(vec3(160.0, 180.0, 0.0).xy());
        let id = spawn_human(&mut ctx.g, house).unwrap();
        let (position, sidewalk_point, start, route) = {
            let map = ctx.g.map();
            let lane = map.lanes().values().find(|lane| lane.kind == LaneKind::Driving).unwrap();
            let sidewalk = map.roads()[lane.parent].sidewalks(lane.src).outgoing.unwrap();
            let offset = lane.points.length() * 0.5;
            let center = lane.points.point_along(offset);
            let sidewalk_point = map.get(sidewalk).unwrap().points.project(center);
            let position = center + (sidewalk_point - center).normalize_to(1.19);
            let (pos, dir) = lane.points.point_dir_along(offset - 4.2);
            let route = Itinerary::route_between_lanes(
                Tick(ctx.g.get_tick()), &map, (lane.id, offset - 4.2), (lane.id, offset + 25.0),
            ).unwrap();
            (position, sidewalk_point, Transform::new_dir(pos, dir), route)
        };
        let goal = route.end_pos().unwrap();
        let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, start).unwrap();
        ctx.g.world.vehicles[car].it = route;
        let collider = put_pedestrian_in_transport_grid(&mut ctx.g.write::<TransportGrid>(), position);
        ctx.g.world.edit_human(id, |human| {
            human.location = Location::Outside;
            human.trans.pos = position;
            human.collider = Some(collider);
            human.it = Itinerary::NONE;
        });
        let tick = ctx.g.get_tick();
        let map = ctx.g.map().clone();
        ctx.g.world.edit_human(id, |human| {
            assert!(!human.it.prepare_egress(&map, position, tick));
        });
        let mut schedule = Simulation::schedule();
        schedule.retain_systems(|name| matches!(name,
            "pedestrian_decision_system" | "vehicle_decision_system"
            | "transport_grid_synchronize" | "itinerary_update"
        ));
        for _ in 0..30 {
            ctx.g.tick(&mut schedule, &[]);
        }
        assert_eq!(ctx.g.world.humans[id].collider, Some(collider));
        assert!(!ctx.g.world.vehicles[car].it.has_ended(0.0));

        let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
        for sim in [&mut ctx.g, &mut resumed] {
            for _ in 0..1_000 {
                sim.tick(&mut schedule, &[]);
            }
            assert!(sim.world.humans[id].trans.pos.is_close(sidewalk_point, 0.1));
            assert!(sim.world.humans[id].collider.is_none());
            assert!(sim.world.vehicles[car].trans.pos.is_close(goal, 0.1));
        }
    }

    #[test]
    fn p7_life_unresolved_midblock_keeps_the_pedestrian_protected() {
        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), vec3(470.0, 180.0, 0.0)]);
        let house = ctx.build_house_near(vec3(160.0, 180.0, 0.0).xy());
        let id = spawn_human(&mut ctx.g, house).unwrap();
        let position = ctx
            .g
            .map()
            .lanes()
            .values()
            .find(|lane| lane.kind == LaneKind::Driving)
            .unwrap()
            .points
            .middle();
        let collider = put_pedestrian_in_transport_grid(&mut ctx.g.write::<TransportGrid>(), position);
        ctx.g.world.edit_human(id, |human| {
            human.location = Location::Outside;
            human.trans.pos = position;
            human.collider = Some(collider);
            human.it = Itinerary::NONE;
        });

        let tick = ctx.g.get_tick();
        let map = ctx.g.map().clone();
        ctx.g.world.edit_human(id, |human| {
            assert!(!human.it.prepare_egress(&map, position, tick));
        });
        let mut schedule = Simulation::schedule();
        schedule.retain_systems(|name| {
            matches!(name, "pedestrian_decision_system" | "itinerary_update")
        });
        for _ in 0..(60 * EGRESS_RECHECK_TICKS) {
            ctx.g.tick(&mut schedule, &[]);
        }
        let human = &ctx.g.world.humans[id];
        assert!(human.it.is_pedestrian_egress());
        assert!(human.it.get_point().is_none());
        assert_eq!(human.trans.pos, position);
        assert_eq!(human.speed.0, 0.0);
        assert!(ctx.g.read::<TransportGrid>().get(collider.0).is_some());

        // A valid routed handoff at the same pose can finish normally.
        ctx.g.world.edit_human(id, |human| {
            human.it = Itinerary::simple(vec![position]);
        });
        let next_tick = ctx.g.get_tick() + 1;
        update_at(&mut ctx.g, next_tick);
        assert!(ctx.g.world.humans[id].collider.is_none());
    }

    #[test]
    fn p7_life_vehicle_exit_hands_off_to_the_scheduled_walk() {
        use crate::map_dynamic::{Destination, RoutingStep};
        use crate::transportation::spawn_driving_vehicle;
        use geom::Transform;

        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), vec3(470.0, 180.0, 0.0)]);
        let house = ctx.build_house_near(vec3(160.0, 180.0, 0.0).xy());
        let id = spawn_human(&mut ctx.g, house).unwrap();
        let (trans, destination) = {
            let map = ctx.g.map();
            let lane = map.lanes().values().find(|lane| lane.kind == LaneKind::Driving).unwrap();
            let road = map.get(lane.parent).unwrap();
            let sidewalk = road.lanes_iter()
                .find_map(|(id, kind)| (kind == LaneKind::Walking).then(|| map.get(id).unwrap()))
                .unwrap();
            (
                Transform::new_dir(lane.points.middle(), lane.points.last_dir().unwrap()),
                sidewalk.points.middle(),
            )
        };
        let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, trans).unwrap();
        ctx.g.world.edit_human(id, |human| {
            human.location = Location::Vehicle(car);
            human.it = Itinerary::NONE;
            human.router.set_test_route(
                Destination::Outside(destination),
                vec![
                    RoutingStep::DriveTo(car, trans.pos),
                    RoutingStep::GetOutVehicle(car),
                    RoutingStep::WalkTo(destination),
                ],
            );
        });
        let mut schedule = Simulation::schedule();
        schedule.retain_systems(|name| {
            matches!(name, "routing_update_system" | "pedestrian_decision_system" | "itinerary_update")
        });
        ctx.g.tick(&mut schedule, &[]);
        let human = &ctx.g.world.humans[id];
        assert_eq!(human.location, Location::Outside);
        assert!(matches!(human.router.cur_step(), Some(RoutingStep::GetOutVehicle(vehicle)) if *vehicle == car));
        assert!(!human.it.is_pedestrian_egress());
        ctx.g.tick(&mut schedule, &[]);
        let human = &ctx.g.world.humans[id];
        assert!(matches!(human.router.cur_step(), Some(RoutingStep::WalkTo(point)) if *point == destination));
        assert!(!human.it.is_pedestrian_egress());
    }

    #[test]
    fn u5_throughput_resumes_mid_crosswalk_and_backward_corner() {
        #[derive(Serialize)]
        struct LegacyItinerary<'a> {
            kind: &'a ItineraryKind,
            reversed_local_path: &'a [Vec3],
        }

        let mut ctx = TestCtx::new();
        let junction = vec3(270.0, 180.0, 0.0);
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), junction, vec3(470.0, 180.0, 0.0)]);
        ctx.build_roads(&[junction, vec3(270.0, 350.0, 0.0)]);
        let house = ctx.build_house_near(vec3(160.0, 180.0, 0.0).xy());
        let human = spawn_human(&mut ctx.g, house).unwrap();
        let tick = ctx.g.get_tick();

        for (turn_kind, dir, expected) in [
            (TurnKind::Crosswalk, TraverseDirection::Forward, [0; 4]),
            (TurnKind::Crosswalk, TraverseDirection::Backward, [0; 4]),
            (
                TurnKind::WalkingCorner,
                TraverseDirection::Backward,
                [0, 0, 0, 1],
            ),
        ] {
            let (position, itinerary, from, to) = {
                let map = ctx.g.map();
                let intersection = map
                    .intersections()
                    .values()
                    .find(|inter| inter.pos.xy() == junction.xy())
                    .unwrap();
                let turn = intersection
                    .turns()
                    .find(|turn| turn.kind == turn_kind)
                    .unwrap();
                let cur = Traversable::new(TraverseKind::Turn(turn.id), dir);
                let origin = map
                    .get(if dir == TraverseDirection::Forward {
                        turn.id.src
                    } else {
                        turn.id.dst
                    })
                    .unwrap();
                let destination = map.get(cur.destination_lane()).unwrap();
                assert_eq!(
                    origin.parent == destination.parent,
                    turn_kind == TurnKind::Crosswalk
                );
                let end_pos = destination.points.middle();
                let itinerary = Itinerary {
                    kind: ItineraryKind::Route(
                        Route {
                            reversed_route: vec![Traversable::new(
                                TraverseKind::Lane(destination.id),
                                destination.dir_from(intersection.id),
                            )],
                            cur,
                            end_pos,
                            target: ItineraryTarget::Position(end_pos),
                        },
                        PathKind::Pedestrian,
                    ),
                    reversed_local_path: vec![destination.get_inter_node_pos(intersection.id)],
                    ..Itinerary::default()
                };
                (
                    turn.points.middle(),
                    itinerary,
                    origin.parent,
                    destination.parent,
                )
            };
            let legacy = LegacyItinerary {
                kind: &itinerary.kind,
                reversed_local_path: itinerary.local_path(),
            };
            assert_eq!(
                Bincode::encode(&itinerary).unwrap(),
                Bincode::encode(&legacy).unwrap()
            );
            let walker = &mut ctx.g.world.humans[human];
            walker.location = Location::Outside;
            walker.trans.pos = position;
            walker.it = itinerary;
            walker.speed.0 = 10_000.0;

            let mut resumed: Simulation =
                Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
            assert_eq!(
                resumed.read::<RoadThroughput>().current_window(to),
                ([0; 4], 0)
            );
            update_at(&mut resumed, tick + 1);
            assert!(resumed.world.humans[human].it.has_ended(0.0));
            let through = resumed.read::<RoadThroughput>();
            assert_eq!(
                through.current_window(to),
                (expected, 1),
                "{turn_kind:?} {dir:?}"
            );
            assert_eq!(through.current_window(from).0, [0; 4]);
        }
    }

    #[test]
    fn u5_throughput_remembers_the_departure_road_during_an_edit_retry() {
        let ctx = TestCtx::new();
        let junction = vec3(270.0, 180.0, 0.0);
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), junction, vec3(470.0, 180.0, 0.0)]);
        ctx.build_roads(&[junction, vec3(270.0, 350.0, 0.0)]);
        let (mut itinerary, start, from, removed, lane) = {
            let map = ctx.g.map();
            let intersection = map
                .intersections()
                .values()
                .find(|inter| inter.pos.xy() == junction.xy())
                .unwrap();
            let turn = intersection
                .turns()
                .filter(|turn| turn.kind == TurnKind::Driving)
                .find(|turn| {
                    let origin = map.get(map.get(turn.id.src).unwrap().parent).unwrap();
                    let middle = turn.points.middle();
                    !origin.lanes_iter().any(|(id, kind)| {
                        kind == LaneKind::Driving && within_lane(map.get(id).unwrap(), middle)
                    })
                })
                .unwrap();
            let from = map.get(turn.id.src).unwrap();
            let to = map.get(turn.id.dst).unwrap();
            let outgoing = map
                .get(from.parent)
                .unwrap()
                .outgoing_lanes_from(intersection.id)
                .iter()
                .find(|(_, kind)| *kind == LaneKind::Driving)
                .unwrap()
                .0;
            let target = ItineraryTarget::DirectedLane(outgoing, 30.0);
            let itinerary = Itinerary {
                kind: ItineraryKind::Route(
                    Route {
                        reversed_route: vec![Traversable::new(
                            TraverseKind::Lane(outgoing),
                            TraverseDirection::Forward,
                        )],
                        cur: Traversable::new(
                            TraverseKind::Turn(turn.id),
                            TraverseDirection::Forward,
                        ),
                        end_pos: map.get(outgoing).unwrap().points.point_along(30.0),
                        target,
                    },
                    PathKind::Vehicle,
                ),
                reversed_local_path: vec![turn.points.last()],
                road_approach: RoadApproach::Tracking(Some(from.parent)),
            };
            (
                itinerary,
                turn.points.middle(),
                from.parent,
                to.parent,
                outgoing,
            )
        };
        {
            let mut map = ctx.g.map_mut();
            map.remove_road(removed).unwrap();
        }
        let map = ctx.g.map();
        assert!(itinerary.get_travers().unwrap().raw_points(&map).is_none());
        assert_eq!(map.get(lane).unwrap().parent, from);
        let reroute =
            |start, target, kind| Itinerary::route_from(Tick(0), start, lane, target, &map, kind);
        let reject_entry = |road, _| panic!("unexpected road entry on {road:?}");
        let pos =
            itinerary.update_with_route(start, 1.0, 0, &map, |_, _, _| None, Some(reject_entry));
        assert_eq!(pos, start);
        let resumed: Itinerary = Bincode::decode(&Bincode::encode(&itinerary).unwrap()).unwrap();
        for mut branch in [itinerary, resumed] {
            for _ in 0..=REROUTE_WAIT_TICKS {
                assert_eq!(
                    branch.update_with_route(pos, 1.0, 0, &map, reroute, Some(reject_entry)),
                    pos
                );
            }
            let arrived =
                branch.update_with_route(pos, 100.0, 0, &map, reroute, Some(reject_entry));
            assert!(within_lane(map.get(lane).unwrap(), arrived));
        }
    }

    #[test]
    fn u5_movement_reaches_lane_matches_the_full_segment_test() {
        // The full test from before the distance early-out.
        fn reference(lane: &Lane, from: Vec3, to: Vec3) -> bool {
            if within_lane(lane, from) || within_lane(lane, to) {
                return true;
            }
            if from == to {
                return false;
            }
            let movement = Segment3::new(from, to);
            let along_movement = movement.vec();
            let radius2 = (lane.kind.width() * 0.5).powi(2);
            lane.points.segments().any(|segment| {
                if movement.project(segment.src).distance2(segment.src) <= radius2
                    || movement.project(segment.dst).distance2(segment.dst) <= radius2
                {
                    return true;
                }
                let along_lane = segment.vec();
                let normal = along_movement.cross(along_lane);
                let normal_sq = normal.mag2();
                let offset = segment.src - from;
                if normal_sq == 0.0 || offset.dot(normal).powi(2) > radius2 * normal_sq {
                    return false;
                }
                let movement_dist = offset.cross(along_lane).dot(normal);
                let lane_dist = offset.cross(along_movement).dot(normal);
                let range = 0.0..=normal_sq;
                range.contains(&movement_dist) && range.contains(&lane_dist)
            })
        }

        let ctx = TestCtx::new();
        // A bend gives lanes with several segments. Far coordinates exercise rounding.
        ctx.build_roads(&[
            vec3(11_400.0, 7_400.0, 0.0),
            vec3(11_520.0, 7_430.0, 0.0),
            vec3(11_600.0, 7_520.0, 0.0),
        ]);
        let map = ctx.g.map();
        let steps = [
            vec3(0.0, 0.0, 0.0),
            vec3(0.4, 0.0, 0.0),
            vec3(0.0, -1.3, 0.0),
            vec3(3.0, 2.0, 0.0),
            vec3(-9.0, 6.5, 0.0),
            vec3(25.0, -25.0, 0.0),
            vec3(-60.0, 10.0, 0.0),
        ];
        let (mut reached, mut missed) = (0, 0);
        for lane in map.lanes().values() {
            let center = lane.points.middle();
            for x in -40..=40 {
                for y in -40..=40 {
                    let from = center + vec3(x as f32 * 1.7, y as f32 * 1.3, 0.0);
                    for step in steps {
                        let to = from + step;
                        let expected = reference(lane, from, to);
                        assert_eq!(
                            movement_reaches_lane(lane, from, to),
                            expected,
                            "lane {:?} from {from:?} to {to:?}",
                            lane.id
                        );
                        if expected {
                            reached += 1;
                        } else {
                            missed += 1;
                        }
                    }
                }
            }
        }
        assert!(
            reached > 1000 && missed > 1000,
            "{reached} reached, {missed} missed"
        );
    }

    #[test]
    fn u5_throughput_excludes_a_rail_route() {
        let ctx = TestCtx::new();
        let mut map = ctx.g.map_mut();
        let builder = LanePatternBuilder::new().rail(true).one_way(true);
        let [a, b, c] = [70.0, 270.0, 470.0].map(|x| vec3(x, 180.0, 0.0));
        let mut connect = |a, b, pattern| {
            let start = map.project(a, 0.0, ProjectFilter::ALL);
            let end = map.project(b, 0.0, ProjectFilter::ALL);
            map.make_connection(start, end, None, &pattern).unwrap().1
        };
        let from = connect(a, b, builder.build());
        // Distinct limits keep the two track segments from merging.
        let to = connect(b, c, builder.speed_limit(12.0).build());
        let rail_middle = |road: RoadID| {
            let lane = map.get(road).unwrap().lanes_iter().next().unwrap().0;
            map.get(lane).unwrap().points.middle()
        };
        let start = rail_middle(from);
        let end = rail_middle(to);
        let mut itinerary = Itinerary::route(Tick(0), start, end, &map, PathKind::Rail).unwrap();
        let position = itinerary.update_with_route(
            start,
            1000.0,
            0,
            &map,
            |_, _, _| None,
            Some(|road, _| panic!("rail route recorded a road entry on {road:?}")),
        );
        assert_eq!(position, end);
        assert!(itinerary.has_ended(0.0));
    }

    #[test]
    fn u5_throughput_waits_for_lane_geometry_after_an_extrapolated_waypoint() {
        let ctx = TestCtx::new();
        ctx.build_roads(&[
            vec3(70.0, 180.0, 0.0),
            vec3(270.0, 180.0, 0.0),
            vec3(470.0, 180.0, 0.0),
        ]);
        let (road, lane, pos, target) = {
            let map = ctx.g.map();
            let eastbound = |y| {
                map.lanes()
                    .values()
                    .find(|lane| {
                        let a = lane.points.first();
                        let b = lane.points.last();
                        lane.kind == LaneKind::Driving
                            && a.x < y
                            && b.x > y
                            && (a.y - 180.0).abs() < 12.0
                    })
                    .unwrap()
            };
            let from = eastbound(170.0);
            let to = eastbound(370.0);
            (
                from.parent,
                from.id,
                from.points.last() + vec3(0.0, -4.0, 0.0),
                ItineraryTarget::DirectedLane(to.id, 30.0),
            )
        };
        let mut through = RoadThroughput::default();
        let map = ctx.g.map();
        let mut route =
            Itinerary::route_from(Tick(0), pos, lane, target, &map, PathKind::Vehicle).unwrap();
        let waypoint = route.get_point().unwrap();
        assert!(map.get(lane).unwrap().points.project_dist2(waypoint) > 4.0);
        let mut update = |route: &mut Itinerary, pos: Vec3, distance: f32| {
            route.update_with_route(
                pos,
                distance,
                0,
                &map,
                |_, _, _| None,
                Some(|road, _| {
                    through.record_entry(road, RoadUserKind::Truck, 1);
                }),
            )
        };
        let position = update(&mut route, pos, pos.distance(waypoint));
        assert!(position.distance(waypoint) < 0.01);
        assert_eq!(through.recent(road, 1), [0; 4]);
        route.update_with_route(
            position,
            4.0,
            0,
            &map,
            |_, _, _| None,
            Some(|road, _| {
                through.record_entry(road, RoadUserKind::Truck, 2);
            }),
        );
        assert_eq!(through.recent(road, 2), [0, 1, 0, 0]);
    }

    #[test]
    fn u5_throughput_keeps_the_previous_road_across_pedestrian_turns() {
        let ctx = TestCtx::new();
        let junction = vec3(270.0, 180.0, 0.0);
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), junction, vec3(470.0, 180.0, 0.0)]);
        ctx.build_roads(&[junction, vec3(270.0, 350.0, 0.0)]);
        let map = ctx.g.map();
        let sidewalks: Vec<_> = map
            .lanes()
            .values()
            .filter(|lane| {
                lane.kind == LaneKind::Walking
                    && [lane.src, lane.dst]
                        .into_iter()
                        .any(|id| map.get(id).unwrap().pos.xy() == junction.xy())
            })
            .collect();
        assert_eq!(sidewalks.len(), 6);
        let (start, mut route, destination, crosswalk) = sidewalks
            .iter()
            .find_map(|from| {
                sidewalks.iter().find_map(|to| {
                    if from.parent == to.parent {
                        return None;
                    }
                    let start = from.points.middle();
                    let route = Itinerary::route_from(
                        Tick(0),
                        start,
                        from.id,
                        ItineraryTarget::Position(to.points.middle()),
                        &map,
                        PathKind::Pedestrian,
                    )?;
                    route
                        .get_route()?
                        .reversed_route
                        .iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .windows(3)
                        .find_map(|legs| match (legs[0].kind, legs[1].kind, legs[2].kind) {
                            (
                                TraverseKind::Turn(_),
                                TraverseKind::Turn(turn),
                                TraverseKind::Lane(lane),
                            ) if lane == to.id
                                && map.get(turn.src).unwrap().parent == to.parent
                                && map.get(turn.dst).unwrap().parent == to.parent =>
                            {
                                Some(turn)
                            }
                            _ => None,
                        })
                        .map(|crosswalk| (start, route, to.parent, crosswalk))
                })
            })
            .expect("a route through a corner and then a crosswalk at the T-junction");
        let mut position = start;
        for _ in 0..1000 {
            position = route.update_with_route(
                position,
                0.5,
                0,
                &map,
                |_, _, _| None,
                Some(|road, _| {
                    panic!("recorded an entry before reaching the sidewalk on {road:?}");
                }),
            );
            if route.get_travers().unwrap().kind == TraverseKind::Turn(crosswalk) {
                break;
            }
        }
        assert_eq!(
            route.get_travers().unwrap().kind,
            TraverseKind::Turn(crosswalk)
        );
        let resumed: Itinerary = Bincode::decode(&Bincode::encode(&route).unwrap()).unwrap();
        for mut branch in [route, resumed] {
            let mut entries = Vec::new();
            branch.update_with_route(
                position,
                1000.0,
                0,
                &map,
                |_, _, _| None,
                Some(|road, _| {
                    entries.push(road);
                }),
            );
            assert_eq!(entries, [destination]);
        }
    }

    #[test]
    fn u5_first_entry_turn_and_multiple_roads_in_one_tick() {
        let mut ctx = TestCtx::new();
        let existing: BTreeSet<_> = ctx.g.map().roads().keys().collect();
        ctx.build_roads(&[
            vec3(60.0, 150.0, 0.0),
            vec3(200.0, 150.0, 0.0),
            vec3(330.0, 220.0, 0.0),
            vec3(450.0, 220.0, 0.0),
        ]);
        let (roads, lanes) = {
            let map = ctx.g.map();
            let mut roads: Vec<_> = map
                .roads()
                .values()
                .filter(|road| !existing.contains(&road.id))
                .collect();
            roads.sort_by(|a, b| a.points.first().x.total_cmp(&b.points.first().x));
            assert_eq!(roads.len(), 3);
            let lanes: Vec<_> = roads
                .iter()
                .map(|road| {
                    road.lanes_iter()
                        .find_map(|(id, kind)| {
                            let lane = map.get(id)?;
                            (kind == LaneKind::Driving
                                && lane.points.first().x < lane.points.last().x)
                                .then_some(id)
                        })
                        .unwrap()
                })
                .collect();
            (roads.iter().map(|road| road.id).collect::<Vec<_>>(), lanes)
        };
        let (pos, route) = {
            let map = ctx.g.map();
            let pos = map.get(lanes[0]).unwrap().points.point_along(20.0) + vec3(0.0, -9.0, 0.0);
            let route = Itinerary::route_from(
                Tick(0),
                pos,
                lanes[0],
                ItineraryTarget::DirectedLane(lanes[2], 30.0),
                &map,
                PathKind::Vehicle,
            )
            .unwrap();
            (pos, route)
        };
        let truck = spawn_truck_at_depot(&mut ctx.g, pos);
        ctx.g.world.vehicles[truck].it = route;
        let tick = ctx.g.get_tick();

        update_at(&mut ctx.g, tick + 1);
        assert_eq!(ctx.g.read::<RoadThroughput>().recent(roads[0], 1), [0; 4]);

        ctx.g.world.vehicles[truck].speed.0 = 50.0;
        let mut turn_tick = None;
        for offset in 2..200 {
            update_at(&mut ctx.g, tick + offset);
            if ctx.g.world.vehicles[truck]
                .it
                .get_travers()
                .is_some_and(|t| matches!(t.kind, TraverseKind::Turn(_)))
            {
                turn_tick = Some(offset);
                break;
            }
        }
        let turn_tick = turn_tick.expect("truck entered the first turn");
        assert_eq!(
            ctx.g.read::<RoadThroughput>().recent(roads[0], turn_tick),
            [0, 1, 0, 0]
        );
        assert_eq!(
            ctx.g.read::<RoadThroughput>().recent(roads[1], turn_tick),
            [0; 4]
        );

        ctx.g.world.vehicles[truck].speed.0 = 50_000.0;
        update_at(&mut ctx.g, tick + turn_tick + 1);
        for road in roads {
            let now = elapsed_ticks(&ctx.g.resources).0;
            assert_eq!(
                ctx.g.read::<RoadThroughput>().recent(road, now),
                [0, 1, 0, 0]
            );
        }
    }

    #[test]
    fn u5_reroute_and_same_road_lane_change_keep_the_prior_road() {
        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), vec3(270.0, 180.0, 0.0)]);
        let (road, lane, opposite, pos) = {
            let map = ctx.g.map();
            let road = map
                .roads()
                .values()
                .find(|r| r.points.first().y == 180.0)
                .unwrap();
            let mut driving = road
                .lanes_iter()
                .filter_map(|(id, kind)| (kind == LaneKind::Driving).then_some(id));
            let lane = driving.next().unwrap();
            let opposite = driving.next().unwrap();
            let pos = map.get(lane).unwrap().points.point_along(15.0);
            (road.id, lane, opposite, pos)
        };
        let truck = spawn_truck_at_depot(&mut ctx.g, pos);
        ctx.g.world.vehicles[truck].it = Itinerary::wait_for_reroute_to_lane(lane, 30.0);
        ctx.g.world.vehicles[truck].speed.0 = 20.0;
        let tick = ctx.g.get_tick();
        update_at(&mut ctx.g, tick + 1);
        assert!(ctx.g.world.vehicles[truck].it.get_route().is_some());
        update_at(&mut ctx.g, tick + 2);

        let vehicle = &mut ctx.g.world.vehicles[truck];
        let itinerary = &mut vehicle.it;
        let ItineraryKind::Route(route, _) = &mut itinerary.kind else {
            panic!("expected route");
        };
        route.target = ItineraryTarget::DirectedLane(opposite, 30.0);
        route.reversed_route.push(Traversable::new(
            TraverseKind::Lane(opposite),
            TraverseDirection::Forward,
        ));
        vehicle.speed.0 = 10_000.0;
        update_at(&mut ctx.g, tick + 3);
        assert_eq!(
            ctx.g
                .read::<RoadThroughput>()
                .recent(road, elapsed_ticks(&ctx.g.resources).0),
            [0; 4]
        );
    }

    #[test]
    fn u5_passengers_are_excluded_from_lane_entries() {
        let mut ctx = TestCtx::new();
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), vec3(290.0, 180.0, 0.0)]);
        let house = ctx.build_house_near(vec3(160.0, 180.0, 0.0).xy());
        let human = spawn_human(&mut ctx.g, house).unwrap();
        let (road, start, end) = {
            let map = ctx.g.map();
            let road = map
                .roads()
                .values()
                .find(|r| r.points.first().y == 180.0)
                .unwrap();
            let (lane, _) = road
                .lanes_iter()
                .find(|&(_, kind)| kind == LaneKind::Walking)
                .unwrap();
            let points = &map.get(lane).unwrap().points;
            let start = points.point_along(15.0) + vec3(0.0, 5.0, 0.0);
            (road.id, start, points.point_along(50.0))
        };
        let truck = spawn_truck_at_depot(&mut ctx.g, start);
        let tick = ctx.g.get_tick();
        for (offset, location, expected) in [
            (1, Location::Vehicle(truck), [0; 4]),
            (2, Location::Transit(truck), [0; 4]),
            (3, Location::Outside, [0, 0, 0, 1]),
        ] {
            let itinerary =
                Itinerary::route(Tick(tick), start, end, &ctx.g.map(), PathKind::Pedestrian)
                    .unwrap();
            let human = &mut ctx.g.world.humans[human];
            human.location = location;
            human.trans.pos = start;
            human.speed.0 = 2_000.0;
            human.it = itinerary;
            update_at(&mut ctx.g, tick + offset);
            assert_eq!(
                ctx.g
                    .read::<RoadThroughput>()
                    .recent(road, elapsed_ticks(&ctx.g.resources).0),
                expected
            );
        }
    }

    #[test]
    fn reroute_reuses_failure_until_road_edit() {
        let mut sim = TestCtx::new();
        let a = vec3(100.0, 100.0, 0.0);
        let b = vec3(200.0, 100.0, 0.0);
        let c = vec3(400.0, 100.0, 0.0);
        let d = vec3(500.0, 100.0, 0.0);
        sim.build_roads(&[a, b]);
        sim.build_roads(&[c, d]);
        let start = (a + b) * 0.5;
        let end = (c + d) * 0.5;
        let car = spawn_truck_at_depot(&mut sim.g, start);
        sim.g.world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Pedestrian, end);

        let first = sim.g.get_tick() + 1;
        ROUTE_ATTEMPTS.with(|attempts| attempts.set(0));
        for tick in first..=first + 201 {
            update_at(&mut sim.g, tick);
        }
        assert_eq!(
            sim.g.world.vehicles[car].it.is_wait_for_reroute(),
            Some(200)
        );
        assert_eq!(ROUTE_ATTEMPTS.with(|attempts| attempts.get()), 1);

        let save = Bincode::encode(&sim.g).unwrap();
        let mut resumed = TestCtx::new();
        resumed.g = Bincode::decode(&save).unwrap();
        assert_eq!(sim.g.hashes(), resumed.g.hashes());
        for tick in first + 202..=first + 402 {
            update_at(&mut sim.g, tick);
            update_at(&mut resumed.g, tick);
        }
        assert_eq!(sim.g.hashes(), resumed.g.hashes());
        assert_eq!(ROUTE_ATTEMPTS.with(|attempts| attempts.get()), 2);

        for branch in [&mut sim, &mut resumed] {
            branch.build_roads(&[b, c]);
            for tick in first + 403..=first + 603 {
                update_at(&mut branch.g, tick);
            }
            assert_eq!(branch.g.world.vehicles[car].it.end_pos(), Some(end));
        }
        assert_eq!(sim.g.hashes(), resumed.g.hashes());
    }
}

#[cfg(test)]
mod pathpark_sweep_tests {
    use super::*;
    use crate::map::LaneKind;
    use crate::tests::pathpark_lifecycle::road_chain;
    use crate::transportation::{spawn_driving_vehicle, DrivewayPhase, DrivewayTraffic};

    #[test]
    fn pathpark_guard_blocks_recovering_walker() {
        let (mut ctx, roads, portal) = road_chain(1);
        let approach = portal.approach(&ctx.g).unwrap();
        let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, approach).unwrap();
        portal.hold(&mut ctx.g, car).unwrap();
        ctx.tick_fast();
        let traffic = ctx.g.read::<DrivewayTraffic>();
        assert_eq!(traffic.phase(portal), Some(DrivewayPhase::Entering));
        let map = ctx.g.map();
        let road = &map.roads()[roads[0]];
        let sidewalks = road.sidewalks(road.src);
        let (curb, exit, start) = [sidewalks.incoming, sidewalks.outgoing]
            .into_iter()
            .flatten()
            .find_map(|curb| {
                let lane = &map.lanes()[curb];
                assert_eq!(lane.kind, LaneKind::Walking);
                (0..180).find_map(|i| {
                    let exit = lane.points.point_along(i as f32 * 0.5);
                    let along = approach.dir;
                    let across = along.xy().perpendicular().z0();
                    [along, -along, across, -across]
                        .into_iter()
                        .find_map(|dir| {
                            let start = exit + dir * 5.;
                            traffic
                                .blocks_step(None, 0.3, start, exit)
                                .then_some((curb, exit, start))
                        })
                })
            })
            .expect("recovery intersects the committed ingress");
        let mut walking = Itinerary {
            kind: ItineraryKind::PedestrianEgress(PedestrianEgress {
                crossing: Some(Traversable::new(
                    TraverseKind::Lane(curb),
                    TraverseDirection::Forward,
                )),
                retry: None,
                check_at: Tick(0),
            }),
            reversed_local_path: vec![exit],
            ..Itinerary::default()
        };
        let mut held = false;
        let movement = |held| DrivewayMovement {
            traffic: &traffic,
            actor: None,
            half_extent: 0.3,
            held,
        };
        let reached = walking.update_guarded::<true>(
            start,
            10.,
            0,
            &map,
            |_, _, _| None,
            None::<fn(RoadID, PathKind)>,
            None,
            |_, _, _| true,
            Some(movement(&mut held)),
        );
        assert_eq!(
            reached, start,
            "pedestrian recovery enters the committed sweep"
        );
        assert!(held);
    }

    #[test]
    fn pathpark_guard_uses_bus_extent() {
        let (mut ctx, _, portal) = road_chain(1);
        let approach = portal.approach(&ctx.g).unwrap();
        let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, approach).unwrap();
        portal.hold(&mut ctx.g, car).unwrap();
        ctx.tick_fast();
        let traffic = ctx.g.read::<DrivewayTraffic>();
        assert_eq!(traffic.phase(portal), Some(DrivewayPhase::Entering));
        let start = approach.pos - approach.dir * 8.;
        let end = approach.pos - approach.dir * 5.5;
        let mut bus = Itinerary::simple(vec![end]);
        let mut held = false;
        let reached = bus.update_guarded::<true>(
            start,
            10.,
            0,
            &ctx.g.map(),
            |_, _, _| None,
            None::<fn(RoadID, PathKind)>,
            None,
            |_, _, _| true,
            Some(DrivewayMovement {
                traffic: &traffic,
                actor: Some(crate::VehicleID::default()),
                half_extent: VehicleKind::Bus.width() * 0.5,
                held: &mut held,
            }),
        );
        assert_eq!(reached, start, "the bus body reaches the committed sweep");
        assert!(held);
    }
}

mod pathpark_route {
    use super::{driving_lane_points, Itinerary, ItineraryKind, ItineraryTarget, Route};
    use crate::map::{
        LaneID, Map, PathKind, RoadID, Traversable, TraverseDirection, TraverseKind, TurnID,
        TurnKind,
    };
    use crate::world_command::bounded_command_vec;
    use geom::{PolyLine3, Transform, Vec3};
    use serde::{Deserialize, Serialize};
    use std::collections::{BTreeMap, BTreeSet};

    const MAX_NODES: usize = 256;
    const MAX_WORK: usize = 2_048;
    const MAX_ROUTE_STEPS: usize = MAX_NODES * 2;
    const MAX_ENDPOINT_POINTS: usize = 512;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DirectedEndpoint {
        pub lane: LaneID,
        pub offset_cm: u32,
    }

    impl DirectedEndpoint {
        pub fn offset(self) -> f32 {
            self.offset_cm as f32 * 0.01
        }

        pub fn transform(self, map: &Map) -> Option<Transform> {
            let (pos, dir) = self.points(map)?.point_dir_along(self.offset());
            Some(Transform::new_dir(pos, dir))
        }

        fn points(self, map: &Map) -> Option<&PolyLine3> {
            driving_lane_points(map, self.lane, self.offset())
                .filter(|points| points.len() <= MAX_ENDPOINT_POINTS)
        }
    }

    pub enum DirectedSearchResult {
        Deferred,
        Unavailable,
        BudgetExhausted,
        Feasible(DirectedPlan),
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct DirectedPlan {
        from: DirectedEndpoint,
        to: DirectedEndpoint,
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_ROUTE_STEPS>")]
        reversed_route: Vec<Traversable>,
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_ENDPOINT_POINTS>")]
        local: Vec<Vec3>,
        end_pos: Vec3,
    }

    impl DirectedPlan {
        pub(crate) fn origin(&self) -> DirectedEndpoint {
            self.from
        }

        pub(crate) fn roads<'a>(&'a self, map: &'a Map) -> impl Iterator<Item = RoadID> + 'a {
            std::iter::once(self.from.lane)
                .chain(
                    self.reversed_route
                        .iter()
                        .filter_map(|step| match step.kind {
                            TraverseKind::Lane(lane) => Some(lane),
                            TraverseKind::Turn(_) => None,
                        }),
                )
                .filter_map(|lane| map.get(lane).map(|lane| lane.parent))
        }

        pub(crate) fn valid(&self, map: &Map) -> bool {
            let (Some(from), Some(to)) = (self.from.points(map), self.to.points(map)) else {
                return false;
            };
            if !self.reversed_route.len().is_multiple_of(2) {
                return false;
            }
            let mut lane = self.from.lane;
            for pair in self.reversed_route.rchunks_exact(2) {
                let TraverseKind::Turn(turn) = pair[1].kind else {
                    return false;
                };
                if pair[0] != lane_step(turn.dst)
                    || pair[1].dir != TraverseDirection::Forward
                    || turn.src != lane
                    || turn.bidirectional
                    || map
                        .get(turn.parent)
                        .and_then(|joint| joint.find_turn(turn))
                        .is_none_or(|turn| turn.kind != TurnKind::Driving)
                {
                    return false;
                }
                lane = turn.dst;
            }
            if lane != self.to.lane
                || (self.reversed_route.is_empty() && self.from.offset_cm > self.to.offset_cm)
            {
                return false;
            }

            let mut expected = Trace::new(None);
            let same_lane = self.reversed_route.is_empty();
            while !expected.materialize(from, to, self.from, self.to, same_lane) {}
            expected.end_pos == Some(self.end_pos) && expected.local == self.local
        }

        pub fn itinerary(&self, map: &Map) -> Option<Itinerary> {
            self.valid(map).then(|| self.clone().into_itinerary())
        }

        pub(crate) fn retained_work(&self) -> usize {
            self.local.len() + self.reversed_route.len() + 1
        }

        pub(crate) fn into_itinerary(self) -> Itinerary {
            Itinerary {
                kind: ItineraryKind::Route(
                    Route {
                        reversed_route: self.reversed_route,
                        end_pos: self.end_pos,
                        cur: lane_step(self.from.lane),
                        target: ItineraryTarget::DirectedLane(self.to.lane, self.to.offset()),
                    },
                    PathKind::Vehicle,
                ),
                reversed_local_path: self.local,
                ..Itinerary::default()
            }
        }
    }

    fn lane_step(id: LaneID) -> Traversable {
        Traversable::new(TraverseKind::Lane(id), TraverseDirection::Forward)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    struct Arrival {
        cost: u64,
        previous: Option<LaneID>,
        through: TurnID,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    struct Edges {
        lane: Option<LaneID>,
        after: Option<TurnID>,
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Sampling {
        index: usize,
        distance: f32,
    }

    impl Sampling {
        fn next(&mut self, points: &PolyLine3) -> (Vec3, Vec3, f32, f32, bool) {
            let a = points[self.index];
            let b = points.get(self.index + 1).copied().unwrap_or(a);
            let start = self.distance;
            self.distance += a.distance(b);
            self.index += 1;
            (a, b, start, self.distance, self.index + 1 >= points.len())
        }
    }

    fn interpolate(a: Vec3, b: Vec3, start: f32, end: f32, offset: f32) -> Vec3 {
        if offset <= start {
            a
        } else if offset >= end {
            b
        } else {
            a + (b - a) * ((offset - start) / (end - start))
        }
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Trace {
        lane: Option<LaneID>,
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_ROUTE_STEPS>")]
        reversed_route: Vec<Traversable>,
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_ENDPOINT_POINTS>")]
        local: Vec<Vec3>,
        end_pos: Option<Vec3>,
        end: Sampling,
        start: Sampling,
        local_done: bool,
        swaps: usize,
    }

    impl Trace {
        fn new(lane: Option<LaneID>) -> Self {
            Self {
                lane,
                reversed_route: Vec::new(),
                local: Vec::new(),
                end_pos: None,
                end: Sampling::default(),
                start: Sampling::default(),
                local_done: false,
                swaps: 0,
            }
        }

        // One destination sample, one clipped origin segment or one swap per call.
        fn materialize(
            &mut self,
            from_points: &PolyLine3,
            to_points: &PolyLine3,
            from: DirectedEndpoint,
            to: DirectedEndpoint,
            same_lane: bool,
        ) -> bool {
            if self.end_pos.is_none() {
                let (a, b, start, end, last) = self.end.next(to_points);
                if to.offset() <= end || last {
                    self.end_pos = Some(interpolate(a, b, start, end, to.offset()));
                }
                return false;
            }
            if !self.local_done {
                let limit = if same_lane {
                    to.offset()
                } else {
                    from_points.length()
                };
                let (a, b, start, end, last) = self.start.next(from_points);
                if from.offset() <= end || last {
                    if self.local.is_empty() {
                        self.local
                            .push(interpolate(a, b, start, end, from.offset()));
                    }
                    if limit > from.offset() && limit > start {
                        self.local.push(interpolate(a, b, start, end, limit));
                    }
                }
                self.local_done = limit <= end || last;
                return false;
            }
            if self.swaps < self.local.len() / 2 {
                let opposite = self.local.len() - 1 - self.swaps;
                self.local.swap(self.swaps, opposite);
                self.swaps += 1;
                return false;
            }
            true
        }
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(try_from = "SavedSearch", into = "SavedSearch")]
    pub struct DirectedSearch {
        from: DirectedEndpoint,
        to: DirectedEndpoint,
        revision: Option<(u64, u64)>,
        frontier: BTreeSet<(u64, LaneID)>,
        arrivals: BTreeMap<LaneID, Arrival>,
        edges: Option<Edges>,
        trace: Option<Trace>,
        spent: usize,
        revision_work: usize,
        cleanup_work: usize,
    }

    #[derive(Serialize, Deserialize)]
    struct SavedSearch {
        from: DirectedEndpoint,
        to: DirectedEndpoint,
        revision: Option<(u64, u64)>,
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_NODES>")]
        frontier: Vec<(u64, LaneID)>,
        #[serde(deserialize_with = "bounded_command_vec::<_, _, MAX_NODES>")]
        arrivals: Vec<(LaneID, Arrival)>,
        edges: Option<Edges>,
        trace: Option<Trace>,
        spent: usize,
        revision_work: usize,
        cleanup_work: usize,
    }

    impl From<DirectedSearch> for SavedSearch {
        fn from(s: DirectedSearch) -> Self {
            Self {
                from: s.from,
                to: s.to,
                revision: s.revision,
                frontier: s.frontier.into_iter().collect(),
                arrivals: s.arrivals.into_iter().collect(),
                edges: s.edges,
                trace: s.trace,
                spent: s.spent,
                revision_work: s.revision_work,
                cleanup_work: s.cleanup_work,
            }
        }
    }

    impl TryFrom<SavedSearch> for DirectedSearch {
        type Error = &'static str;

        fn try_from(s: SavedSearch) -> Result<Self, Self::Error> {
            if s.spent > MAX_WORK
                || !s.frontier.windows(2).all(|p| p[0] < p[1])
                || !s.arrivals.windows(2).all(|p| p[0].0 < p[1].0)
                || s.revision_work.saturating_add(s.cleanup_work) > s.spent
                || s.trace.as_ref().is_some_and(|trace| {
                    trace.start.index > MAX_ENDPOINT_POINTS
                        || trace.end.index > MAX_ENDPOINT_POINTS
                        || trace.swaps > trace.local.len() / 2
                        || trace.end_pos.is_some_and(|p| !p.is_finite())
                        || trace.local.iter().any(|p| !p.is_finite())
                })
            {
                return Err("invalid directed route cursor");
            }
            let arrivals: BTreeMap<_, _> = s.arrivals.into_iter().collect();
            let known = |lane: Option<LaneID>| lane.is_none_or(|id| arrivals.contains_key(&id));
            if s.frontier
                .iter()
                .any(|&(cost, id)| !arrivals.get(&id).is_some_and(|a| a.cost == cost))
                || s.edges.is_some_and(|edges| !known(edges.lane))
                || s.trace.as_ref().is_some_and(|trace| !known(trace.lane))
                || arrivals.iter().any(|(&id, a)| {
                    a.through.dst != id
                        || a.through.bidirectional
                        || a.previous.map_or(a.through.src != s.from.lane, |prev| {
                            a.through.src != prev
                                || !arrivals.get(&prev).is_some_and(|p| p.cost < a.cost)
                        })
                })
            {
                return Err("inconsistent directed route cursor");
            }
            Ok(Self {
                from: s.from,
                to: s.to,
                revision: s.revision,
                frontier: s.frontier.into_iter().collect(),
                arrivals,
                edges: s.edges,
                trace: s.trace,
                spent: s.spent,
                revision_work: s.revision_work,
                cleanup_work: s.cleanup_work,
            })
        }
    }

    impl DirectedSearch {
        pub fn new(from: DirectedEndpoint, to: DirectedEndpoint) -> Self {
            Self {
                from,
                to,
                revision: None,
                frontier: BTreeSet::new(),
                arrivals: BTreeMap::new(),
                edges: Some(Edges {
                    lane: None,
                    after: None,
                }),
                trace: None,
                spent: 0,
                revision_work: 0,
                cleanup_work: 0,
            }
        }

        pub(crate) fn endpoints(&self) -> (DirectedEndpoint, DirectedEndpoint) {
            (self.from, self.to)
        }

        pub(crate) fn stale(&self, map: &Map) -> bool {
            self.revision
                .is_some_and(|previous| previous != map.directed_route_revision())
        }

        pub(crate) fn validate_loaded(&self, map: &Map, ready: Option<&DirectedPlan>) -> bool {
            if self.stale(map) {
                return true; // Advance discards an obsolete graph cursor within the same lifetime.
            }
            let mut rebuilt = Self::new(self.from, self.to);
            let mut work = self.revision_work;
            let rebuilt_plan = match rebuilt.advance(map, &mut work) {
                DirectedSearchResult::Feasible(plan) => Some(plan),
                _ => None,
            };
            rebuilt.spent = self.spent;
            rebuilt == *self && rebuilt_plan.as_ref() == ready
        }

        pub(crate) fn charge(&mut self, budget: &mut usize) -> bool {
            if *budget == 0 || self.spent == MAX_WORK {
                return false;
            }
            *budget -= 1;
            self.spent += 1;
            true
        }

        pub(crate) fn exhausted(&self) -> bool {
            self.spent == MAX_WORK
        }

        pub fn advance(&mut self, map: &Map, budget: &mut usize) -> DirectedSearchResult {
            use DirectedSearchResult::*;
            if self.exhausted() {
                return BudgetExhausted;
            }
            if *budget == 0 {
                return Deferred;
            }
            if self.stale(map) {
                let cleanup = self.frontier.len()
                    + self.arrivals.len()
                    + self
                        .trace
                        .as_ref()
                        .map_or(0, |trace| trace.local.len() + trace.reversed_route.len());
                // Debit retained records across slices before releasing the stale cursor.
                while self.cleanup_work < cleanup {
                    if !self.charge(budget) {
                        return if self.exhausted() {
                            BudgetExhausted
                        } else {
                            Deferred
                        };
                    }
                    self.cleanup_work += 1;
                }
                *self = Self {
                    spent: self.spent,
                    ..Self::new(self.from, self.to)
                };
            }
            let Some(start) = self.from.points(map) else {
                return Unavailable;
            };
            let Some(end) = self.to.points(map) else {
                return Unavailable;
            };
            if self.revision.is_none() {
                if !self.charge(budget) {
                    return if self.exhausted() {
                        BudgetExhausted
                    } else {
                        Deferred
                    };
                }
                self.revision_work += 1;
                self.revision = Some(map.directed_route_revision());
                if self.from.lane == self.to.lane && self.from.offset_cm <= self.to.offset_cm {
                    self.trace = Some(Trace::new(None));
                }
            }
            while self.charge(budget) {
                self.revision_work += 1;
                if let Some(trace) = &mut self.trace {
                    if let Some(lane) = trace.lane {
                        if trace.reversed_route.len() == MAX_ROUTE_STEPS {
                            return BudgetExhausted;
                        }
                        let arrival = self.arrivals[&lane];
                        trace.reversed_route.push(lane_step(lane));
                        trace.reversed_route.push(Traversable::new(
                            TraverseKind::Turn(arrival.through),
                            TraverseDirection::Forward,
                        ));
                        trace.lane = arrival.previous;
                        continue;
                    }
                    let same_lane = trace.reversed_route.is_empty();
                    if trace.materialize(start, end, self.from, self.to, same_lane) {
                        let trace = self.trace.take().unwrap();
                        return Feasible(DirectedPlan {
                            from: self.from,
                            to: self.to,
                            reversed_route: trace.reversed_route,
                            local: trace.local,
                            end_pos: trace.end_pos.unwrap(),
                        });
                    }
                    continue;
                }
                if let Some(edges) = &mut self.edges {
                    let lane_id = edges.lane.unwrap_or(self.from.lane);
                    let Some(lane) = map.get(lane_id) else {
                        return Unavailable;
                    };
                    let Some(turn) = map
                        .get(lane.dst)
                        .and_then(|joint| joint.next_turn(edges.after))
                    else {
                        self.edges = None;
                        continue;
                    };
                    edges.after = Some(turn.id);
                    if turn.kind != TurnKind::Driving || turn.id.src != lane_id {
                        continue;
                    }
                    let Some(next) = map
                        .get(turn.id.dst)
                        .filter(|l| l.kind == crate::map::LaneKind::Driving)
                    else {
                        continue;
                    };
                    let previous_cost = edges.lane.map_or(0, |id| self.arrivals[&id].cost);
                    let distance =
                        lane.points.length() - edges.lane.map_or(self.from.offset(), |_| 0.0);
                    let seconds = (distance + turn.points.length()) / lane.speed_limit;
                    let cost = previous_cost + ((seconds * 1_000.0).ceil() as u64).max(1);
                    if let Some(old) = self.arrivals.get(&next.id) {
                        if old.cost <= cost {
                            continue;
                        }
                        self.frontier.remove(&(old.cost, next.id));
                    } else if self.arrivals.len() == MAX_NODES {
                        return BudgetExhausted;
                    }
                    self.arrivals.insert(
                        next.id,
                        Arrival {
                            cost,
                            previous: edges.lane,
                            through: turn.id,
                        },
                    );
                    self.frontier.insert((cost, next.id));
                } else {
                    let Some((_, lane)) = self.frontier.pop_first() else {
                        return Unavailable;
                    };
                    if lane == self.to.lane {
                        self.trace = Some(Trace::new(Some(lane)));
                    } else {
                        self.edges = Some(Edges {
                            lane: Some(lane),
                            after: None,
                        });
                    }
                }
            }
            if self.exhausted() {
                BudgetExhausted
            } else {
                Deferred
            }
        }
    }
}

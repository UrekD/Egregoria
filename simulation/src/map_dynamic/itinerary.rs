use crate::map::{
    LaneID, LaneKind, Map, PathKind, Pathfinder, RoadID, Traversable, TraverseDirection,
    TraverseKind,
};
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::ParkingManagement;
use crate::transportation::{RoadThroughput, RoadUserKind};
use crate::utils::resources::Resources;
use crate::world::TrainID;
use crate::World;
use egui_inspect::egui::Ui;
use egui_inspect::{Inspect, InspectArgs};
use geom::{Follower, PolyLine3, Polyline3Queue, Vec3};
use prototypes::{GameTime, Tick, DELTA};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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

#[derive(Default, Debug, Serialize, Deserialize, Inspect)]
pub struct Itinerary {
    kind: ItineraryKind,
    reversed_local_path: Vec<Vec3>,
    #[inspect(skip)]
    #[serde(skip)]
    road_approach: RoadApproach,
}

#[derive(Debug, Default)]
enum RoadApproach {
    #[default]
    Unchecked,
    Joining(RoadID, PathKind),
    Done,
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
    map_revision: (u64, u64),
    entries: BTreeSet<RerouteKey>,
}

impl RerouteFailures {
    fn sync_map(&mut self, map: &Map) {
        let revision = (map.identity(), map.walking_topology_generation);
        if self.map_revision != revision {
            self.entries.clear();
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
                driving_lane_points(map, lane, offset)?.point_along(offset),
            ),
        };

        let mut cur = Traversable::new(TraverseKind::Lane(start_lane), TraverseDirection::Forward);

        if start_lane == end_lane {
            let local_path = match target {
                ItineraryTarget::Position(_) => pathkind
                    .local_route(map, start_lane, start, end)
                    .map(PolyLine3::into_vec),
                ItineraryTarget::DirectedLane(lane, offset) => {
                    directed_local_path(driving_lane_points(map, lane, offset)?, start, offset, end)
                }
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

    fn advance(
        &mut self,
        map: &Map,
        position: Vec3,
        on_road_entry: &mut Option<impl FnMut(RoadID, PathKind)>,
    ) -> Option<Vec3> {
        let v = self.reversed_local_path.pop();

        if self.reversed_local_path.is_empty() {
            if let ItineraryKind::Route(ref mut r, pathkind) = self.kind {
                let previous = r.cur;
                r.cur = r.reversed_route.pop()?;

                let points = match r.cur.points(map) {
                    Some(x) => x,
                    None => {
                        *self = Self::retry(pathkind, r.target);
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
                            let path = driving_lane_points(map, lane, offset).and_then(|points| {
                                directed_local_path(points, points.first(), offset, r.end_pos)
                            });
                            let Some(path) = path else {
                                *self = Self::retry(pathkind, r.target);
                                return None;
                            };
                            path
                        }
                    };
                } else {
                    self.reversed_local_path = points.into_vec();
                }
                self.reversed_local_path.reverse();

                if let Some(record) = on_road_entry
                    .as_mut()
                    .filter(|_| pathkind != PathKind::Rail)
                {
                    if let TraverseKind::Lane(id) = r.cur.kind {
                        let previous_lane = match previous.kind {
                            TraverseKind::Lane(lane) => lane,
                            TraverseKind::Turn(turn) => match previous.dir {
                                TraverseDirection::Forward => turn.src,
                                TraverseDirection::Backward => turn.dst,
                            },
                        };
                        if let Some(lane) = map
                            .get(id)
                            .filter(|lane| pathkind.authorized_lane(lane.kind))
                        {
                            if map.get(previous_lane).map(|previous| previous.parent)
                                != Some(lane.parent)
                            {
                                record(lane.parent, pathkind);
                            }
                        }
                    }
                }
            }
        }
        v
    }

    fn initial_road_approach(&self, map: &Map, position: Vec3) -> RoadApproach {
        let ItineraryKind::Route(route, kind) = &self.kind else {
            return RoadApproach::Done;
        };
        let TraverseKind::Lane(id) = route.cur.kind else {
            return RoadApproach::Done;
        };
        let Some(lane) = map
            .get(id)
            .filter(|lane| *kind != PathKind::Rail && kind.authorized_lane(lane.kind))
        else {
            return RoadApproach::Done;
        };
        let Some(road) = map.get(lane.parent) else {
            return RoadApproach::Done;
        };

        // A route can start in a driveway, a parking bay or next to a sidewalk.
        // A reroute from any travel lane of this same segment is already on it.
        if road.lanes_iter().any(|(id, lane_kind)| {
            kind.authorized_lane(lane_kind)
                && map.get(id).is_some_and(|lane| {
                    let half_width = lane_kind.width() * 0.5;
                    lane.points.project_dist2(position) <= half_width * half_width
                })
        }) {
            RoadApproach::Done
        } else {
            RoadApproach::Joining(road.id, *kind)
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
        mut position: Vec3,
        mut dist_to_move: f32,
        time: u32,
        map: &Map,
        route: impl FnOnce(Vec3, ItineraryTarget, PathKind) -> Option<Self>,
        mut on_road_entry: Option<impl FnMut(RoadID, PathKind)>,
    ) -> Vec3 {
        if let ItineraryKind::Route(ref active, kind) = self.kind {
            if matches!(active.target, ItineraryTarget::DirectedLane(..))
                && active.cur.raw_points(map).is_none()
            {
                *self = Self::retry(kind, active.target);
            }
        }
        if on_road_entry.is_some()
            && matches!(self.road_approach, RoadApproach::Unchecked)
            && !self.reversed_local_path.is_empty()
        {
            self.road_approach = self.initial_road_approach(map, position);
        }
        while let Some(p) = self.get_point() {
            let dist = position.distance(p);
            if dist <= dist_to_move + 0.01 {
                if let RoadApproach::Joining(road, kind) = self.road_approach {
                    if let Some(record) = &mut on_road_entry {
                        if map.get(road).is_some() {
                            record(road, kind);
                        }
                    }
                    self.road_approach = RoadApproach::Done;
                }
                dist_to_move -= dist;
                position = p;
                if self.is_terminal() {
                    self.advance(map, position, &mut on_road_entry);
                    return p;
                }

                if self.remaining_points() > 1 {
                    self.advance(map, position, &mut on_road_entry);
                    continue;
                }

                let k = unwrap_or!(self.get_travers(), {
                    *self = Itinerary::NONE;
                    return p;
                });

                if k.can_pass(time, map.lanes()) {
                    self.advance(map, position, &mut on_road_entry);
                    continue;
                }
                return p;
            }

            return position + (p - position).normalize_to(dist_to_move);
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
            *self = unwrap_or!(route(position, target, kind), {
                *wait_ticks = REROUTE_WAIT_TICKS;
                return position;
            });
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
            ItineraryKind::WaitUntil(_) | ItineraryKind::WaitForReroute { .. } => None,
            ItineraryKind::Simple(e) => Some(e),
            ItineraryKind::Route(ref r, _) => Some(r.end_pos),
        }
    }

    pub fn target(&self) -> Option<ItineraryTarget> {
        match &self.kind {
            ItineraryKind::Route(route, _) => Some(route.target),
            ItineraryKind::WaitForReroute { target, .. } => Some(*target),
            ItineraryKind::Simple(end) => Some(ItineraryTarget::Position(*end)),
            _ => None,
        }
    }

    pub fn remaining_points(&self) -> usize {
        self.reversed_local_path.len()
    }

    pub fn is_terminal(&self) -> bool {
        match &self.kind {
            ItineraryKind::None | ItineraryKind::WaitUntil(_) => true,
            ItineraryKind::WaitForReroute { .. } => false,
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
            | ItineraryKind::WaitForReroute { .. } => None,
            ItineraryKind::Simple(e) => Some(e),
            ItineraryKind::Route(Route { end_pos, .. }, _) => Some(end_pos),
        }
    }

    pub fn get_travers(&self) -> Option<&Traversable> {
        match &self.kind {
            ItineraryKind::None
            | ItineraryKind::WaitUntil(_)
            | ItineraryKind::Simple(_)
            | ItineraryKind::WaitForReroute { .. } => None,
            ItineraryKind::Route(Route { cur, .. }, _) => Some(cur),
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
            ItineraryKind::WaitForReroute { .. } => false,
            ItineraryKind::Route(
                Route {
                    ref reversed_route, ..
                },
                _,
            ) => reversed_route.is_empty() && self.reversed_local_path.is_empty(),
            _ => self.reversed_local_path.is_empty(),
        }
    }

    pub fn is_none_or_wait(&self) -> bool {
        matches!(self.kind, ItineraryKind::None | ItineraryKind::WaitUntil(_))
    }

    pub fn is_simple(&self) -> bool {
        matches!(self.kind, ItineraryKind::Simple(_))
    }
}

fn driving_lane_points(map: &Map, lane: LaneID, offset: f32) -> Option<&PolyLine3> {
    let lane = map.get(lane)?;
    (lane.kind == LaneKind::Driving && (0.0..=lane.points.length()).contains(&offset))
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
        };
        false
    }
}

pub fn itinerary_update(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::itinerary_update");
    let time = &*resources.read::<GameTime>();
    let map = &*resources.read::<Map>();
    let failures = &mut resources.write::<ParkingManagement>().reroute_failures;
    failures.sync_map(map);
    let mut throughput = resources.write::<RoadThroughput>();
    let tick = elapsed_ticks(resources).0;
    throughput.begin_tick(map, tick);

    world
        .query_it_trans_speed()
        .for_each(|(it, trans, speed, kind)| {
            // A stationary entity with a route may still cross an itinerary
            // boundary. Only the inert itinerary kinds can skip movement here.
            if it.reversed_local_path.is_empty()
                && matches!(it.kind, ItineraryKind::None | ItineraryKind::WaitUntil(_))
            {
                return;
            }
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
            trans.pos = it.update_with_route(
                trans.pos,
                speed * DELTA,
                time.seconds,
                map,
                |start, target, kind| failures.route(time.tick, start, target, map, kind),
                record,
            );
        });

    world.trains.values_mut().for_each(|train| {
        train.leader.past.push(train.trans.pos);
    });

    world.wagons.values_mut().for_each(|wagon| {
        let leader = &unwrap_ret!(world.trains.get(wagon.itfollower.leader)).leader;
        let (pos, dir) = wagon.itfollower.head.update(&leader.past);
        let (pos2, dir2) = wagon.itfollower.tail.update(&leader.past);
        wagon.trans.pos = (pos + pos2) * 0.5;
        wagon.trans.dir = (dir + dir2).try_normalize().unwrap_or(dir);
    });
}

#[cfg(test)]
thread_local! {
    static ROUTE_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestCtx;
    use crate::transportation::spawn_truck_at_depot;
    use crate::Simulation;
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;

    fn update_at(sim: &mut Simulation, tick: u64) {
        *sim.write::<GameTime>() = GameTime::new(Tick(tick));
        itinerary_update(&mut sim.world, &mut sim.resources);
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
        use crate::souls::human::spawn_human;
        use crate::transportation::Location;

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

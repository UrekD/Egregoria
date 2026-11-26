use super::indexed_astar;
use crate::map::{
    Lane, LaneID, LaneKind, LanePatternBuilder, Map, Traversable, TraverseDirection, TraverseKind,
    TurnID,
};
use common::{hash_u64, FastMap};
use geom::{PolyLine3, Vec3};
use ordered_float::OrderedFloat;
use prototypes::Tick;
use serde::{Deserialize, Serialize};
use slotmapd::Key;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

#[path = "../map_dynamic/route_cache.rs"]
mod route_cache;
use route_cache::{walk_state, WalkRouteCache, WalkState};

pub trait Pathfinder {
    fn path(
        &self,
        map: &Map,
        tick: Tick,
        start: Traversable,
        end: LaneID,
    ) -> Option<Vec<Traversable>>;
    fn nearest_lane(&self, map: &Map, pos: Vec3) -> Option<LaneID>;
    fn local_route(&self, map: &Map, lane: LaneID, start: Vec3, end: Vec3) -> Option<PolyLine3>;
    fn authorized_lane(&self, kind: LaneKind) -> bool;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PathKind {
    Pedestrian,
    Vehicle,
    Rail,
}

impl Pathfinder for PathKind {
    fn path(
        &self,
        map: &Map,
        tick: Tick,
        start: Traversable,
        end: LaneID,
    ) -> Option<Vec<Traversable>> {
        match self {
            PathKind::Pedestrian => PedestrianPath.path(map, tick, start, end),
            PathKind::Vehicle => CarPath.path(map, tick, start, end),
            PathKind::Rail => RailPath.path(map, tick, start, end),
        }
    }

    fn nearest_lane(&self, map: &Map, pos: Vec3) -> Option<LaneID> {
        match self {
            PathKind::Pedestrian => PedestrianPath.nearest_lane(map, pos),
            PathKind::Vehicle => CarPath.nearest_lane(map, pos),
            PathKind::Rail => RailPath.nearest_lane(map, pos),
        }
    }

    fn local_route(&self, map: &Map, lane: LaneID, start: Vec3, end: Vec3) -> Option<PolyLine3> {
        match self {
            PathKind::Pedestrian => PedestrianPath.local_route(map, lane, start, end),
            PathKind::Vehicle => CarPath.local_route(map, lane, start, end),
            PathKind::Rail => RailPath.local_route(map, lane, start, end),
        }
    }

    fn authorized_lane(&self, kind: LaneKind) -> bool {
        match self {
            PathKind::Pedestrian => PedestrianPath.authorized_lane(kind),
            PathKind::Vehicle => CarPath.authorized_lane(kind),
            PathKind::Rail => RailPath.authorized_lane(kind),
        }
    }
}

struct PedestrianPath;

const MAX_WALK_NODES: usize = 192 * 1024;
const MAX_WALK_EDGES: usize = 512 * 1024;
const MAX_WALK_TURN_CHECKS: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub(super) struct WalkingRoutes {
    searched: AtomicBool,
    graph: OnceLock<Option<WalkGraph>>,
    cache: Mutex<WalkRouteCache>,
}

impl WalkingRoutes {
    fn path(&self, map: &Map, start: Traversable, end: LaneID) -> Option<Vec<Traversable>> {
        let key = (walk_state(start), end);
        if let Some(route) = self.cache.lock().unwrap().get(key) {
            return route;
        }

        // An isolated first request after edit/load can use the live search.
        // Build the graph only when a later cache miss can use it.
        let graph = if self.searched.swap(true, Ordering::Relaxed) {
            self.graph.get_or_init(|| WalkGraph::build(map)).as_ref()
        } else {
            None
        };
        let route = search_walk(map, start, end, graph);
        self.cache.lock().unwrap().insert(key, route.as_deref());
        route
    }

    fn clear_costs(&mut self) {
        *self.cache.get_mut().unwrap() = WalkRouteCache::default();
        *self.searched.get_mut() = self.graph.get().is_some();
        if let Some(Some(graph)) = self.graph.get_mut() {
            graph.costs.take();
        }
    }
}

#[cfg(test)]
thread_local! {
    static WALK_SEARCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Pathfinder for PedestrianPath {
    fn path(
        &self,
        map: &Map,
        _tick: Tick,
        start: Traversable,
        end: LaneID,
    ) -> Option<Vec<Traversable>> {
        let target = map.lanes.get(end)?;
        map.intersections.get(target.dst)?;
        if start.kind == TraverseKind::Lane(end) {
            return Some(vec![start]);
        }
        map.intersections
            .get(start.destination_intersection(&map.lanes)?)?;
        if target.kind != LaneKind::Walking
            || !map
                .lanes
                .get(start.destination_lane())
                .is_some_and(|lane| lane.kind == LaneKind::Walking)
        {
            return search_walk(map, start, end, None);
        }
        map.walking_routes
            .get_or_init(WalkingRoutes::default)
            .path(map, start, end)
    }

    fn nearest_lane(&self, map: &Map, pos: Vec3) -> Option<LaneID> {
        map.nearest_lane(pos, LaneKind::Walking, None)
    }

    fn local_route(&self, map: &Map, lane: LaneID, start: Vec3, end: Vec3) -> Option<PolyLine3> {
        let lane = map.lanes.get(lane)?;
        let (p_start, seg_start) = lane.points.project_segment(start);
        let (p_end, seg_end) = lane.points.project_segment(end);

        let segs = lane
            .points
            .get(seg_start.min(seg_end)..seg_start.max(seg_end))?;
        let mut v = Vec::with_capacity(3 + segs.len());
        v.push(p_start);
        v.extend_from_slice(segs);
        v.push(p_end);
        v.push(end);
        Some(PolyLine3::new(v))
    }

    fn authorized_lane(&self, kind: LaneKind) -> bool {
        matches!(kind, LaneKind::Walking)
    }
}

struct WalkGraph {
    indices: FastMap<WalkState, usize>,
    nodes: Vec<WalkNode>,
    edges: Vec<usize>,
    costs: OnceLock<Vec<WalkCost>>,
}

struct WalkNode {
    traversable: Traversable,
    edges: std::ops::Range<usize>,
}

struct WalkCost {
    position: Option<Vec3>,
    length: f32,
}

impl WalkGraph {
    fn build(map: &Map) -> Option<Self> {
        if map.lanes.len() > MAX_WALK_NODES || map.intersections.len() > MAX_WALK_NODES {
            return None;
        }
        if map
            .intersections
            .values()
            .map(|junction| junction.turns().len())
            .sum::<usize>()
            > MAX_WALK_EDGES
        {
            return None;
        }
        let walking = |lane| {
            map.lanes
                .get(lane)
                .is_some_and(|l| l.kind == LaneKind::Walking)
        };
        let lanes = map
            .lanes
            .iter()
            .filter(|(_, lane)| lane.kind == LaneKind::Walking)
            .map(|(id, _)| TraverseKind::Lane(id));
        let turns = map
            .intersections
            .values()
            .flat_map(|intersection| intersection.turns())
            .filter(|turn| walking(turn.id.src) || walking(turn.id.dst))
            .map(|turn| TraverseKind::Turn(turn.id));
        let mut nodes = Vec::new();
        let mut indices = FastMap::default();
        for kind in lanes.chain(turns) {
            if nodes.len() == MAX_WALK_NODES {
                return None;
            }
            for dir in [TraverseDirection::Forward, TraverseDirection::Backward] {
                let traversable = Traversable::new(kind, dir);
                if indices
                    .insert(walk_state(traversable), nodes.len())
                    .is_some()
                {
                    return None;
                }
                nodes.push(WalkNode {
                    traversable,
                    edges: 0..0,
                });
            }
        }
        let mut edges = Vec::new();
        let mut turn_checks = 0;
        for node in &mut nodes {
            if let Some(junction) = node
                .traversable
                .destination_intersection(&map.lanes)
                .and_then(|id| map.intersections.get(id))
            {
                turn_checks += junction.turns().len();
                if turn_checks > MAX_WALK_TURN_CHECKS {
                    return None;
                }
            }
            let first = edges.len();
            for (successor, _) in walk_successors(map, node.traversable) {
                if edges.len() == MAX_WALK_EDGES {
                    return None;
                }
                // A mixed-lane turn can require states outside this graph.
                edges.push(*indices.get(&walk_state(successor))?);
            }
            node.edges = first..edges.len();
        }
        Some(Self {
            nodes,
            edges,
            indices,
            costs: OnceLock::new(),
        })
    }

    fn path(
        &self,
        map: &Map,
        start: usize,
        end: LaneID,
        end_pos: Vec3,
    ) -> Option<Vec<Traversable>> {
        let end = TraverseKind::Lane(end);
        let forward = *self.indices.get(&(end, true))?;
        let backward = *self.indices.get(&(end, false))?;
        let costs = self.costs.get_or_init(|| {
            self.nodes
                .iter()
                .map(|node| WalkCost {
                    position: walk_position(map, node.traversable),
                    length: match node.traversable.kind {
                        TraverseKind::Lane(lane) => map.lanes[lane].points.length(),
                        TraverseKind::Turn(_) => WALK_TURN_COST,
                    },
                })
                .collect()
        });
        let path = indexed_astar::astar(
            self.nodes.len(),
            start,
            |node| {
                self.edges[self.nodes[node].edges.clone()]
                    .iter()
                    .map(|&next| (next, costs[next].length))
            },
            |node| walk_heuristic(costs[node].position, end_pos),
            |node| node == forward || node == backward,
        )?;
        Some(
            path.into_iter()
                .map(|node| self.nodes[node].traversable)
                .collect(),
        )
    }
}

fn walk_position(map: &Map, t: Traversable) -> Option<Vec3> {
    Some(
        map.intersections
            .get(t.destination_intersection(&map.lanes)?)?
            .pos,
    )
}

fn walk_heuristic(pos: Option<Vec3>, end_pos: Vec3) -> f32 {
    pos.map_or(f32::INFINITY, |pos| pos.distance(end_pos) * 1.3)
}

const WALK_TURN_COST: f32 = 0.001;

fn walk_successors(map: &Map, t: Traversable) -> impl Iterator<Item = (Traversable, f32)> + '_ {
    let inter = t
        .destination_intersection(&map.lanes)
        .and_then(|x| map.intersections.get(x));
    let lane_from_id = t.destination_lane();
    let lane_from = map.lanes.get(lane_from_id);
    let lane_travers = inter.zip(lane_from).map(|(inter, lane_from)| {
        (
            Traversable::new(
                TraverseKind::Lane(lane_from_id),
                lane_from.dir_from(inter.id),
            ),
            lane_from.points.length(),
        )
    });
    inter
        .into_iter()
        .flat_map(move |inter| {
            inter.turns_from(lane_from_id).map(|(turn, dir)| {
                (
                    Traversable::new(TraverseKind::Turn(turn), dir),
                    WALK_TURN_COST,
                )
            })
        })
        .chain(lane_travers)
}

fn search_walk(
    map: &Map,
    start: Traversable,
    end: LaneID,
    graph: Option<&WalkGraph>,
) -> Option<Vec<Traversable>> {
    let lane = map.lanes.get(end)?;
    let end_pos = map.intersections.get(lane.dst)?.pos;
    #[cfg(test)]
    WALK_SEARCHES.with(|n| n.set(n.get() + 1));
    if let Some((graph, start)) = graph
        .filter(|_| lane.kind == LaneKind::Walking)
        .and_then(|graph| Some((graph, *graph.indices.get(&walk_state(start))?)))
    {
        return graph.path(map, start, end, end_pos);
    }
    pathfinding::directed::astar::astar(
        &start,
        |&t| walk_successors(map, t).map(|(next, cost)| (next, OrderedFloat(cost))),
        |&t| OrderedFloat(walk_heuristic(walk_position(map, t), end_pos)),
        |t| t.kind == TraverseKind::Lane(end),
    )
    .map(|(path, _)| path)
}

struct RailPath;

impl Pathfinder for RailPath {
    fn path(
        &self,
        map: &Map,
        tick: Tick,
        start: Traversable,
        end: LaneID,
    ) -> Option<Vec<Traversable>> {
        CarPath.path(map, tick, start, end)
    }

    fn nearest_lane(&self, map: &Map, pos: Vec3) -> Option<LaneID> {
        map.nearest_lane(pos, LaneKind::Rail, None)
    }

    fn local_route(&self, map: &Map, lane: LaneID, start: Vec3, end: Vec3) -> Option<PolyLine3> {
        CarPath.local_route(map, lane, start, end)
    }

    fn authorized_lane(&self, kind: LaneKind) -> bool {
        matches!(kind, LaneKind::Rail)
    }
}

struct CarPath;

pub(super) fn lane_turn_successors(map: &Map, lane: LaneID) -> impl Iterator<Item = LaneID> + '_ {
    map.lanes
        .get(lane)
        .and_then(|l| map.intersections.get(l.dst))
        .into_iter()
        .flat_map(move |inter| inter.turns_from(lane).map(|(turn, _)| turn.dst))
}

pub(super) fn driving_lane_cost(lane: &Lane, seed: u32) -> f32 {
    lane.points.length() / lane.speed_limit
        + common::rand::randu(lane.dist_from_bottom.to_bits() ^ seed)
}

pub(super) fn driving_heuristic(pos: Option<Vec3>, end_pos: Vec3) -> f32 {
    const HEURISTIC_SPEED: f32 = LanePatternBuilder::new().speed_limit;
    pos.map_or(f32::INFINITY, |position| {
        position.distance(end_pos) * 1.2 / HEURISTIC_SPEED
    })
}

pub(super) fn search_driving(
    map: &Map,
    start: LaneID,
    end: LaneID,
    seed: u32,
) -> Option<Vec<LaneID>> {
    let end_pos = map.intersections.get(map.lanes.get(end)?.dst)?.pos;
    let root = LaneID::null();
    pathfinding::directed::astar::astar(
        &root,
        |&lane| {
            let lane = if lane == root { start } else { lane };
            lane_turn_successors(map, lane).map(|next| {
                let cost = map
                    .lanes
                    .get(next)
                    .map_or(f32::INFINITY, |lane| driving_lane_cost(lane, seed));
                (next, OrderedFloat(cost))
            })
        },
        |&lane| {
            let pos = map
                .lanes
                .get(lane)
                .and_then(|lane| map.intersections.get(lane.dst))
                .map(|junction| junction.pos);
            OrderedFloat(driving_heuristic(pos, end_pos))
        },
        |&lane| lane == end,
    )
    .map(|(path, _)| path.into_iter().skip(1).collect())
}

impl Pathfinder for CarPath {
    fn path(
        &self,
        map: &Map,
        tick: Tick,
        start: Traversable,
        end: LaneID,
    ) -> Option<Vec<Traversable>> {
        let inters = &map.intersections;
        let lanes = &map.lanes;

        let start_lane = start.destination_lane();

        inters.get(lanes.get(end)?.dst)?;
        // Avoid building the reachability index when a cold query has no successors.
        if map.driving_reachability.get().is_none()
            && lane_turn_successors(map, start_lane).next().is_none()
        {
            return None;
        }
        let reachability = map.driving_reachability();
        let can_drive = reachability.can_reach(start_lane, end);
        #[cfg(not(debug_assertions))]
        if !can_drive {
            return None;
        }

        let base_random = hash_u64((start_lane.data().as_ffi(), tick.0)) as u32;
        let route = reachability.path(map, start_lane, end, base_random);
        #[cfg(debug_assertions)]
        if !can_drive {
            assert!(
                route.is_none(),
                "reachability rejected a route from {start_lane:?} to {end:?}",
            );
        }
        let route = route?;
        let mut path = Vec::with_capacity(route.len() * 2 + 1);
        path.push(start);

        let mut last_id = start_lane;

        for lane in route {
            let inter_end = &inters.get(lanes.get(lane)?.src)?;
            let id = TurnID::new(inter_end.id, last_id, lane, false);
            path.push(Traversable::new(
                TraverseKind::Turn(id),
                TraverseDirection::Forward,
            ));
            path.push(Traversable::new(
                TraverseKind::Lane(lane),
                TraverseDirection::Forward,
            ));

            last_id = lane;
        }
        Some(path)
    }

    fn nearest_lane(&self, map: &Map, pos: Vec3) -> Option<LaneID> {
        map.nearest_lane(pos, LaneKind::Driving, None)
    }

    fn local_route(&self, map: &Map, lane: LaneID, start: Vec3, end: Vec3) -> Option<PolyLine3> {
        let lane = &map.lanes.get(lane)?;
        let (p_start, seg_start) = lane.points.project_segment(start);
        let (p_end, seg_end) = lane.points.project_segment(end);

        if seg_end < seg_start
            || (seg_end == seg_start
                && lane.points.get(seg_end)?.distance2(p_start)
                    < lane.points.get(seg_end)?.distance2(p_end))
        {
            return None;
        }

        let segs = &lane.points.get(seg_start..seg_end)?;
        let mut v = Vec::with_capacity(3 + segs.len());
        v.push(p_start);
        v.extend_from_slice(segs);
        v.push(p_end);
        Some(PolyLine3::new(v))
    }

    fn authorized_lane(&self, kind: LaneKind) -> bool {
        matches!(kind, LaneKind::Driving | LaneKind::Bus)
    }
}

impl Map {
    #[allow(dead_code, reason = "wired by the map edit handoff")]
    pub(super) fn lane_costs_changed(&mut self) {
        if let Some(routes) = self.walking_routes.get_mut() {
            routes.clear_costs();
        }
    }

    fn driving_reachability(&self) -> &super::reachability::DrivingReachability {
        self.driving_reachability
            .get_or_init(|| super::reachability::DrivingReachability::build(self))
    }

    pub(crate) fn can_drive_between(&self, start: LaneID, end: LaneID) -> bool {
        self.driving_reachability().can_reach(start, end)
    }

    /// Labels the driving component of a lane. Lanes with the same label reach
    /// each other both ways. Labels last until the lane topology changes.
    pub(crate) fn driving_component(&self, lane: LaneID) -> Option<usize> {
        self.driving_reachability().cyclic_component(lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{
        procgen, IntersectionID, LanePatternBuilder, MapProject, ProjectKind, RoadID,
        RoadSegmentKind, Turn, TurnKind,
    };
    use common::saveload::{Bincode, Encoder};
    use std::collections::{BTreeSet, VecDeque};

    fn reachable_lanes(map: &Map, start: LaneID) -> BTreeSet<LaneID> {
        let mut reached = BTreeSet::new();
        let mut pending = VecDeque::from([start]);
        while let Some(lane) = pending.pop_front() {
            if let Some(intersection) = map
                .lanes
                .get(lane)
                .and_then(|l| map.intersections.get(l.dst))
            {
                for (turn, _) in intersection.turns_from(lane) {
                    if reached.insert(turn.dst) {
                        pending.push_back(turn.dst);
                    }
                }
            }
        }
        reached
    }

    fn check_random_pairs(map: &Map, kind: LaneKind) {
        let lanes: Vec<_> = map
            .lanes()
            .values()
            .filter(|lane| lane.kind == kind)
            .map(|lane| lane.id)
            .collect();
        let path_kind = if kind == LaneKind::Rail {
            PathKind::Rail
        } else {
            PathKind::Vehicle
        };
        let mut rng = common::rand::gen(42);
        let mut sample = || lanes[(rng.next_f32() * lanes.len() as f32) as usize];
        for _ in 0..32 {
            let start = sample();
            let reachable = reachable_lanes(map, start);
            for end in std::iter::once(start).chain((0..32).map(|_| sample())) {
                let expected = reachable.contains(&end);
                let may_reach = map.can_drive_between(start, end);
                assert!(
                    may_reach || !expected,
                    "{kind:?}: precheck rejected {start:?} -> {end:?}",
                );
                if kind != LaneKind::Driving || may_reach != expected {
                    assert_eq!(
                        path_kind
                            .path(map, Tick(0), forward_lane(start), end)
                            .is_some(),
                        expected,
                        "{kind:?}: {start:?} -> {end:?}",
                    );
                }
            }
        }
    }

    fn forward_lane(lane: LaneID) -> Traversable {
        Traversable::new(TraverseKind::Lane(lane), TraverseDirection::Forward)
    }

    #[test]
    fn route_perf_reuses_walk_searches_and_invalidates_on_edit() {
        let mut map = Map::empty();
        let a = map.add_intersection(geom::vec3(0.0, 0.0, 0.3));
        let b = map.add_intersection(geom::vec3(100.0, 0.0, 0.3));
        let c = map.add_intersection(geom::vec3(200.0, 0.0, 0.3));
        let d = map.add_intersection(geom::vec3(300.0, 0.0, 0.3));
        let pattern = LanePatternBuilder::new().build();
        let mut connect = |a, b| {
            map.connect(a, b, &pattern, RoadSegmentKind::Straight)
                .unwrap()
        };
        let (ab, bc, cd) = (connect(a, b), connect(b, c), connect(c, d));
        for intersection in [a, b, c, d] {
            map.update_intersection(intersection, |_| {});
        }
        let sidewalk = |road| {
            map.roads[road]
                .lanes_iter()
                .find(|&(_, kind)| kind == LaneKind::Walking)
                .unwrap()
                .0
        };
        let (start, end) = (forward_lane(sidewalk(ab)), sidewalk(cd));
        let before = Bincode::encode(&map).unwrap();
        WALK_SEARCHES.with(|n| n.set(0));
        let route = PathKind::Pedestrian
            .path(&map, Tick(0), start, end)
            .unwrap();
        assert_eq!(
            PathKind::Pedestrian.path(&map, Tick(500), start, end),
            Some(route)
        );
        assert_eq!(WALK_SEARCHES.with(|n| n.get()), 1);
        assert_eq!(before, Bincode::encode(&map).unwrap());

        map.remove_road(bc).unwrap();
        assert_eq!(PathKind::Pedestrian.path(&map, Tick(501), start, end), None);
        let searches = WALK_SEARCHES.with(|n| n.get());
        assert_eq!(PathKind::Pedestrian.path(&map, Tick(502), start, end), None);
        assert_eq!(WALK_SEARCHES.with(|n| n.get()), searches);

        map.connect(b, c, &pattern, RoadSegmentKind::Straight)
            .unwrap();
        for id in [b, c] {
            map.update_intersection(id, |_| {});
        }
        let expected = search_walk(&map, start, end, None);
        assert!(expected.is_some(), "reconnected route must exist");
        let loaded = Bincode::decode::<Map>(&Bincode::encode(&map).unwrap()).unwrap();
        for map in [&map, &loaded] {
            assert_eq!(
                PathKind::Pedestrian.path(map, Tick(503), start, end),
                expected
            );
            assert_eq!(
                PathKind::Pedestrian.path(map, Tick(504), start, end),
                expected
            );
        }
    }

    #[test]
    fn route_perf_indexed_routes_match_the_reference_search() {
        let mut map = Map::empty();
        procgen::load_parismap(&mut map).unwrap();
        let walking = WalkGraph::build(&map).unwrap();
        let driving = map.driving_reachability();
        let mut rng = common::rand::gen(42);

        for kind in [LaneKind::Walking, LaneKind::Driving] {
            let lanes: Vec<_> = map
                .lanes
                .values()
                .filter(|lane| lane.kind == kind)
                .map(|lane| lane.id)
                .collect();
            let mut sample = || lanes[(rng.next_f32() * lanes.len() as f32) as usize];
            for tick in 0..256 {
                let start = sample();
                let end = if tick % 8 == 0 { start } else { sample() };
                if kind == LaneKind::Walking {
                    for dir in [TraverseDirection::Forward, TraverseDirection::Backward] {
                        let start = Traversable::new(TraverseKind::Lane(start), dir);
                        assert_eq!(
                            search_walk(&map, start, end, Some(&walking)),
                            search_walk(&map, start, end, None)
                        );
                    }
                } else {
                    let seed = hash_u64((start.data().as_ffi(), tick as u64)) as u32;
                    assert_eq!(
                        driving.path(&map, start, end, seed),
                        search_driving(&map, start, end, seed),
                        "{start:?} -> {end:?}, tick {tick}"
                    );
                }
            }
        }

        let walks = map
            .lanes
            .iter()
            .filter(|(_, l)| l.kind == LaneKind::Walking)
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let turns = map
            .intersections
            .values()
            .flat_map(|i| i.turns())
            .filter(|turn| map.lanes[turn.id.src].kind == LaneKind::Walking)
            .take(128);
        for (i, turn) in turns.enumerate() {
            let end = walks[i * 13 % walks.len()];
            for dir in [TraverseDirection::Forward, TraverseDirection::Backward] {
                let start = Traversable::new(TraverseKind::Turn(turn.id), dir);
                assert_eq!(
                    search_walk(&map, start, end, Some(&walking)),
                    search_walk(&map, start, end, None),
                    "ordered traversal from {start:?} to {end:?}",
                );
            }
        }
    }

    // Construct exact ties without relying on turn generation or road geometry.
    // The map codec permits orphan lanes and turns pointing to a removed lane.
    fn tied_driving_map(stale_turn: bool, kind: LaneKind) -> (Map, LaneID, LaneID) {
        let mut map = Map::empty();
        let junction = map.add_intersection(Vec3::ZERO);
        let mut insert = || {
            map.lanes.insert_with_key(|id| crate::map::Lane {
                id,
                parent: RoadID::null(),
                src: junction,
                dst: junction,
                kind,
                control: crate::map::TrafficControl::Always,
                speed_limit: 10.0,
                points: PolyLine3::new(vec![Vec3::ZERO, Vec3::X * 100.0]),
                dist_from_bottom: 0.0,
            })
        };
        let [start, dead_end, first, second, removed, end] = std::array::from_fn(|_| insert());
        let mut connections = vec![
            (start, dead_end),
            (start, first),
            (start, second),
            (first, end),
            (second, end),
        ];
        if stale_turn {
            connections.push((start, removed));
        }
        map.lanes.remove(removed);
        let turns: BTreeSet<_> = connections
            .into_iter()
            .map(|(from, to)| Turn::new(TurnID::new(junction, from, to, false), TurnKind::Driving))
            .collect();
        let intersection = &map.intersections[junction];
        let wire = Bincode::encode(&(
            intersection.id,
            intersection.pos,
            intersection.radius,
            turns,
            &intersection.roads,
            intersection.turn_policy,
            intersection.light_policy,
        ))
        .unwrap();
        map.intersections[junction] = Bincode::decode(&wire).unwrap();
        (map, start, end)
    }

    fn assert_driving_route(
        map: &Map,
        kind: PathKind,
        tick: Tick,
        start: Traversable,
        end: LaneID,
    ) {
        let origin = start.destination_lane();
        let seed = hash_u64((origin.data().as_ffi(), tick.0)) as u32;
        let expected = search_driving(map, origin, end, seed).map(|lanes| {
            let mut steps = vec![start];
            let mut previous = origin;
            for lane in lanes {
                let turn = TurnID::new(map.lanes[lane].src, previous, lane, false);
                steps.push(Traversable::new(
                    TraverseKind::Turn(turn),
                    TraverseDirection::Forward,
                ));
                steps.push(forward_lane(lane));
                previous = lane;
            }
            steps
        });
        assert_eq!(kind.path(map, tick, start, end), expected);
    }

    #[test]
    fn route_perf_loaded_dangling_turn_preserves_tied_route() {
        let (map, start, end) = tied_driving_map(true, LaneKind::Driving);
        let bytes = Bincode::encode(&map).unwrap();
        let loaded: Map = Bincode::decode(&bytes).unwrap();
        assert_eq!(Bincode::encode(&loaded).unwrap(), bytes);
        assert_driving_route(
            &loaded,
            PathKind::Vehicle,
            Tick(0),
            forward_lane(start),
            end,
        );
    }

    #[test]
    fn route_perf_same_lane_ids_use_current_costs() {
        for (kind, path_kind) in [
            (LaneKind::Bus, PathKind::Vehicle),
            (LaneKind::Rail, PathKind::Rail),
        ] {
            let (mut map, start, end) = tied_driving_map(false, kind);
            let start = Traversable::new(TraverseKind::Lane(start), TraverseDirection::Backward);
            let tick = Tick(42);
            let original = path_kind.path(&map, tick, start, end).unwrap();
            assert_driving_route(&map, path_kind, tick, start, end);
            let TraverseKind::Lane(winner) = original[2].kind else {
                panic!("expected the first branch lane");
            };
            map.lanes[winner].speed_limit *= 0.01;
            assert_driving_route(&map, path_kind, tick, start, end);
            assert_ne!(path_kind.path(&map, tick, start, end), Some(original));

            map.lanes[winner].speed_limit *= 100.0;
            map.lanes[winner].points = PolyLine3::new(vec![Vec3::ZERO, Vec3::Z * 2000.0]);
            assert_driving_route(&map, path_kind, tick, start, end);
        }
    }

    #[test]
    fn route_perf_walk_cost_edit_keeps_adjacency_and_refreshes_weights() {
        let (mut map, start, end) = tied_driving_map(false, LaneKind::Walking);
        let start = forward_lane(start);
        let reverse_start = Traversable::new(start.kind, TraverseDirection::Backward);
        let junction = start.destination_intersection(&map.lanes).unwrap();

        let expected = search_walk(&map, start, end, None);
        assert!(expected.is_some());
        assert_eq!(
            PathKind::Pedestrian.path(&map, Tick(0), start, end),
            expected
        );
        assert!(map.walking_routes.get().unwrap().graph.get().is_none());
        PathKind::Pedestrian
            .path(&map, Tick(1), reverse_start, end)
            .unwrap();
        let graph = || {
            map.walking_routes
                .get()
                .unwrap()
                .graph
                .get()
                .unwrap()
                .as_ref()
                .unwrap()
        };
        let edges = graph().edges.as_ptr();
        assert!(graph().costs.get().is_some());

        map.intersections[junction].pos.z += 8.0;
        map.lanes[end].points = PolyLine3::new(vec![Vec3::ZERO, Vec3::Z * 1000.0]);
        map.lane_costs_changed();
        let routes = map.walking_routes.get().unwrap();
        let graph = routes.graph.get().unwrap().as_ref().unwrap();
        assert_eq!(graph.edges.as_ptr(), edges);
        assert!(graph.costs.get().is_none());
        assert!(routes
            .cache
            .lock()
            .unwrap()
            .get((walk_state(start), end))
            .is_none());
        assert_eq!(
            PathKind::Pedestrian.path(&map, Tick(2), start, end),
            search_walk(&map, start, end, None),
        );
        let end_index = graph.indices[&walk_state(forward_lane(end))];
        let costs = graph.costs.get().unwrap();
        assert_eq!(costs[end_index].length, 1000.0);
        assert_eq!(costs[end_index].position.unwrap().z, 8.0);
    }

    #[test]
    fn route_perf_invalid_walk_skips_cold_graph_and_cache() {
        let (map, start, _) = tied_driving_map(false, LaneKind::Walking);
        assert_eq!(
            PathKind::Pedestrian.path(&map, Tick(0), forward_lane(start), LaneID::null()),
            None
        );
        assert!(map.walking_routes.get().is_none());
    }

    #[test]
    fn driving_reachability_matches_bfs_on_paris() {
        let mut map = Map::empty();
        procgen::load_parismap(&mut map);
        check_random_pairs(&map, LaneKind::Driving);
    }

    #[test]
    fn driving_reachability_matches_bfs_on_directed_grid() {
        let mut map = Map::empty();
        let grid: Vec<_> = (0..4)
            .flat_map(|y| (0..4).map(move |x| geom::vec3(100.0 * x as f32, 100.0 * y as f32, 0.3)))
            .map(|pos| map.add_intersection(pos))
            .collect();
        let pattern = LanePatternBuilder::new().one_way(true).build();
        for y in 0..4 {
            for x in 0..4 {
                let i = 4 * y + x;
                if x < 3 {
                    let (a, b) = if y % 2 == 0 { (i, i + 1) } else { (i + 1, i) };
                    map.connect(grid[a], grid[b], &pattern, RoadSegmentKind::Straight)
                        .unwrap();
                }
                if y < 3 {
                    let (a, b) = if y == 0 || x % 2 == 0 {
                        (i + 4, i)
                    } else {
                        (i, i + 4)
                    };
                    map.connect(grid[a], grid[b], &pattern, RoadSegmentKind::Straight)
                        .unwrap();
                }
            }
        }
        for id in grid {
            map.update_intersection(id, |_| {});
        }
        check_random_pairs(&map, LaneKind::Driving);
    }

    #[test]
    fn pfb2a3_reachability_samples_bus_and_rail_lanes() {
        for kind in [LaneKind::Bus, LaneKind::Rail] {
            let mut map = Map::empty();
            let intersections: Vec<_> = (0..6)
                .map(|x| map.add_intersection(geom::vec3(100.0 * x as f32, 0.0, 0.3)))
                .collect();
            let mut pattern = LanePatternBuilder::new()
                .one_way(true)
                .sidewalks(false)
                .parking(false)
                .build();
            pattern.lanes_forward[0].0 = kind;
            for pair in intersections.windows(2) {
                map.connect(pair[0], pair[1], &pattern, RoadSegmentKind::Straight)
                    .unwrap();
            }
            for id in intersections {
                map.update_intersection(id, |_| {});
            }
            check_random_pairs(&map, kind);
            let path_kind = if kind == LaneKind::Rail {
                PathKind::Rail
            } else {
                PathKind::Vehicle
            };
            let lanes = map.lanes.keys().collect::<Vec<_>>();
            for tick in [0, 1, 31, 255] {
                for pair in lanes.windows(2) {
                    assert_driving_route(
                        &map,
                        path_kind,
                        Tick(tick),
                        forward_lane(pair[0]),
                        pair[1],
                    );
                    assert_driving_route(
                        &map,
                        path_kind,
                        Tick(tick),
                        forward_lane(pair[1]),
                        pair[0],
                    );
                }
            }
        }
    }

    #[test]
    fn pfb2a3_same_lane_loop_tracks_turn_policy() {
        let mut map = Map::empty();
        let a = map.add_intersection(geom::vec3(0.0, 0.0, 0.3));
        let b = map.add_intersection(geom::vec3(100.0, 0.0, 0.3));
        let c = map.add_intersection(geom::vec3(200.0, 0.0, 0.3));
        let ab = map
            .connect(
                a,
                b,
                &LanePatternBuilder::new().build(),
                RoadSegmentKind::Straight,
            )
            .unwrap();
        connect_one_way(&mut map, b, c);
        map.update_intersection(a, |_| {});
        let lane = map.roads[ab]
            .outgoing_lanes_from(a)
            .iter()
            .find(|&&(_, kind)| kind == LaneKind::Driving)
            .unwrap()
            .0;
        let start = forward_lane(lane);
        assert_driving_route(&map, PathKind::Vehicle, Tick(0), start, lane);
        assert!(PathKind::Vehicle.path(&map, Tick(0), start, lane).is_none());

        map.update_intersection(b, |junction| junction.turn_policy.back_turns = true);
        assert_driving_route(&map, PathKind::Vehicle, Tick(0), start, lane);
        let path = PathKind::Vehicle.path(&map, Tick(0), start, lane).unwrap();
        assert!(path.len() > 1);
        assert_eq!(path.first(), Some(&start));
        assert_eq!(path.last(), Some(&start));
        let loaded: Map = Bincode::decode(&Bincode::encode(&map).unwrap()).unwrap();
        assert_driving_route(&loaded, PathKind::Vehicle, Tick(0), start, lane);
        assert_eq!(
            PathKind::Vehicle.path(&loaded, Tick(0), start, lane),
            Some(path)
        );

        map.update_intersection(b, |junction| junction.turn_policy.back_turns = false);
        assert_driving_route(&map, PathKind::Vehicle, Tick(0), start, lane);
        assert!(PathKind::Vehicle.path(&map, Tick(0), start, lane).is_none());
    }

    fn connect_one_way(map: &mut Map, src: IntersectionID, dst: IntersectionID) -> RoadID {
        let project = |id| MapProject {
            pos: map.intersections[id].pos,
            kind: ProjectKind::Intersection(id),
        };
        map.make_connection(
            project(src),
            project(dst),
            None,
            &LanePatternBuilder::new().one_way(true).build(),
        )
        .unwrap()
        .1
    }

    #[test]
    fn driving_reachability_tracks_edits_and_load() {
        let mut map = Map::empty();
        let a = map.add_intersection(geom::vec3(0.0, 0.0, 0.3));
        let b = map.add_intersection(geom::vec3(100.0, 0.0, 0.3));
        let ab = connect_one_way(&mut map, a, b);
        let c = map.add_intersection(geom::vec3(100.0, 100.0, 0.3));
        let d = map.add_intersection(geom::vec3(200.0, 100.0, 0.3));
        let cd = connect_one_way(&mut map, c, d);
        let driving_lane = |road: RoadID| {
            map.roads[road]
                .lanes_iter()
                .find(|&(_, kind)| kind == LaneKind::Driving)
                .unwrap()
                .0
        };
        let (start, end) = (driving_lane(ab), driving_lane(cd));
        assert!(!map.can_drive_between(start, end));
        assert!(!map.can_drive_between(start, start));

        let bridge = connect_one_way(&mut map, b, c);
        let bytes = Bincode::encode(&map).unwrap();
        assert!(map.can_drive_between(start, end));
        assert!(!map.can_drive_between(end, start));
        assert_eq!(bytes, Bincode::encode(&map).unwrap());
        let cloned = map.clone();
        assert_ne!(map.identity(), cloned.identity());
        assert_eq!(bytes, Bincode::encode(&cloned).unwrap());
        assert!(cloned.can_drive_between(start, end));
        let loaded = Bincode::decode::<Map>(&bytes).unwrap();
        assert_ne!(map.identity(), loaded.identity());
        assert_ne!(cloned.identity(), loaded.identity());
        assert!(loaded.can_drive_between(start, end));
        assert!(!loaded.can_drive_between(end, start));

        map.remove_road(bridge).unwrap();
        assert!(!map.can_drive_between(start, end));
        assert!(!map.can_drive_between(start, start));
    }
}

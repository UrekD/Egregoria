use super::indexed_astar;
use crate::map::{LaneID, LaneKind, Map, Traversable, TraverseDirection, TraverseKind, TurnID};
use common::hash_u64;
use geom::{PolyLine3, Vec3};
use ordered_float::OrderedFloat;
use prototypes::Tick;
use serde::{Deserialize, Serialize};
use slotmapd::Key;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

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

const MAX_WALK_ROUTES: usize = 8192;
const MAX_CACHED_WALK_LENGTH: usize = 256;

type WalkState = (TraverseKind, bool);
type WalkRouteKey = (WalkState, LaneID);

pub(super) struct WalkingRoutes {
    graph: Option<WalkGraph>,
    cache: Mutex<WalkRouteCache>,
}

#[derive(Default)]
struct WalkRouteCache {
    routes: BTreeMap<WalkRouteKey, Option<Box<[Traversable]>>>,
    insertion_order: VecDeque<WalkRouteKey>,
}

impl WalkingRoutes {
    fn new(map: &Map) -> Self {
        Self {
            graph: WalkGraph::build(map),
            cache: Mutex::default(),
        }
    }

    fn path(&self, map: &Map, start: Traversable, end: LaneID) -> Option<Vec<Traversable>> {
        let key = (walk_state(start), end);
        if let Some(route) = self.cache.lock().unwrap().routes.get(&key) {
            return route.as_deref().map(<[_]>::to_vec);
        }

        let route = search_walk(map, start, end, self.graph.as_ref());
        if route
            .as_ref()
            .is_none_or(|route| route.len() <= MAX_CACHED_WALK_LENGTH)
        {
            let mut cache = self.cache.lock().unwrap();
            // Another reader could have filled the entry during this search.
            if !cache.routes.contains_key(&key) {
                if cache.routes.len() == MAX_WALK_ROUTES {
                    let oldest = cache.insertion_order.pop_front().unwrap();
                    cache.routes.remove(&oldest);
                }
                cache.insertion_order.push_back(key);
                cache.routes.insert(key, route.as_deref().map(Box::from));
            }
        }
        route
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
        map.walking_routes
            .get_or_init(|| WalkingRoutes::new(map))
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
    indices: BTreeMap<WalkState, usize>,
    nodes: Vec<WalkNode>,
    edges: Vec<(usize, f32)>,
}

struct WalkNode {
    traversable: Traversable,
    position: Option<Vec3>,
    edges: std::ops::Range<usize>,
}

impl WalkGraph {
    fn build(map: &Map) -> Option<Self> {
        let walking = |lane| {
            map.lanes
                .get(lane)
                .is_some_and(|l| l.kind == LaneKind::Walking)
        };
        let lanes = map
            .lanes
            .values()
            .filter(|lane| lane.kind == LaneKind::Walking)
            .map(|lane| TraverseKind::Lane(lane.id));
        let turns = map
            .intersections
            .values()
            .flat_map(|intersection| intersection.turns())
            .filter(|turn| walking(turn.id.src) || walking(turn.id.dst))
            .map(|turn| TraverseKind::Turn(turn.id));
        let mut nodes = Vec::new();
        let mut indices = BTreeMap::new();
        for kind in lanes.chain(turns) {
            for dir in [TraverseDirection::Forward, TraverseDirection::Backward] {
                let traversable = Traversable::new(kind, dir);
                indices.insert(walk_state(traversable), nodes.len());
                nodes.push(WalkNode {
                    traversable,
                    position: walk_position(map, traversable),
                    edges: 0..0,
                });
            }
        }
        let mut edges = Vec::new();
        for node in &mut nodes {
            let first = edges.len();
            for (successor, cost) in walk_successors(map, node.traversable) {
                // An unexpected mixed-lane turn in loaded data uses the live
                // graph, which can search arbitrary lanes.
                edges.push((*indices.get(&walk_state(successor))?, cost));
            }
            node.edges = first..edges.len();
        }
        Some(Self {
            nodes,
            edges,
            indices,
        })
    }

    fn path(&self, start: usize, end: LaneID, end_pos: Vec3) -> Option<Vec<Traversable>> {
        let end = TraverseKind::Lane(end);
        let forward = *self.indices.get(&(end, true))?;
        let backward = *self.indices.get(&(end, false))?;
        let path = indexed_astar::astar(
            self.nodes.len(),
            start,
            |node| self.edges[self.nodes[node].edges.clone()].iter().copied(),
            |node| walk_heuristic(self.nodes[node].position, end_pos),
            |node| node == forward || node == backward,
        )?;
        Some(
            path.into_iter()
                .map(|node| self.nodes[node].traversable)
                .collect(),
        )
    }
}

fn walk_state(t: Traversable) -> WalkState {
    (t.kind, t.dir == TraverseDirection::Forward)
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
            inter
                .turns_from(lane_from_id)
                .map(|(turn, dir)| (Traversable::new(TraverseKind::Turn(turn), dir), 0.001))
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
        return graph.path(start, end, end_pos);
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

        let reachability = map.driving_reachability();
        let can_drive = reachability.can_reach(start_lane, end);
        #[cfg(not(debug_assertions))]
        if !can_drive {
            return None;
        }

        let base_random = hash_u64((start_lane.data().as_ffi(), tick.0)) as u32;
        let route = reachability.path(start_lane, end, base_random);
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
        RoadSegmentKind,
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
    }

    #[test]
    fn route_perf_indexed_routes_match_the_reference_search() {
        let mut map = Map::empty();
        procgen::load_parismap(&mut map);
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
                        driving
                            .path(start, end, seed)
                            .map(Iterator::collect::<Vec<_>>),
                        reference_driving_path(&map, start, end, seed),
                        "{start:?} -> {end:?}, tick {tick}"
                    );
                }
            }
        }
    }

    fn reference_driving_path(
        map: &Map,
        start: LaneID,
        end: LaneID,
        seed: u32,
    ) -> Option<Vec<LaneID>> {
        let end_pos = map.intersections[map.lanes[end].dst].pos;
        let root = LaneID::null();
        pathfinding::directed::astar::astar(
            &root,
            |&lane| {
                let lane = if lane == root { start } else { lane };
                lane_turn_successors(map, lane).map(|next| {
                    let lane = &map.lanes[next];
                    let cost = lane.points.length() / lane.speed_limit
                        + common::rand::randu(lane.dist_from_bottom.to_bits() ^ seed);
                    (next, OrderedFloat(cost))
                })
            },
            |&lane| {
                let distance = map.intersections[map.lanes[lane].dst].pos.distance(end_pos);
                OrderedFloat(distance * 1.2 / LanePatternBuilder::new().speed_limit)
            },
            |&lane| lane == end,
        )
        .map(|(path, _)| path.into_iter().skip(1).collect())
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
        assert!(PathKind::Vehicle.path(&map, Tick(0), start, lane).is_none());

        map.update_intersection(b, |junction| junction.turn_policy.back_turns = true);
        let path = PathKind::Vehicle.path(&map, Tick(0), start, lane).unwrap();
        assert!(path.len() > 1);
        assert_eq!(path.first(), Some(&start));
        assert_eq!(path.last(), Some(&start));

        map.update_intersection(b, |junction| junction.turn_policy.back_turns = false);
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

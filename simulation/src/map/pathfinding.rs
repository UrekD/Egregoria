use crate::map::{
    LaneID, LaneKind, LanePatternBuilder, Map, Traversable, TraverseDirection, TraverseKind, TurnID,
};
use common::hash_u64;
use geom::{PolyLine3, Vec3};
use ordered_float::OrderedFloat;
use prototypes::Tick;
use serde::{Deserialize, Serialize};
use slotmapd::Key;

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

impl Pathfinder for PedestrianPath {
    fn path(
        &self,
        map: &Map,
        _tick: Tick,
        start: Traversable,
        end: LaneID,
    ) -> Option<Vec<Traversable>> {
        let inters = &map.intersections;
        let lanes = &map.lanes;

        let end_pos = inters.get(lanes.get(end)?.dst)?.pos;

        let heuristic = |t: &Traversable| {
            let pos = unwrap_ret!(
                inters.get(unwrap_ret!(
                    t.destination_intersection(lanes),
                    OrderedFloat(f32::INFINITY)
                )),
                OrderedFloat(f32::INFINITY)
            )
            .pos;

            OrderedFloat(pos.distance(end_pos) * 1.3) // Inexact but (much) faster
        };

        let successors = |t: &Traversable| {
            let inter = t
                .destination_intersection(lanes)
                .and_then(|x| inters.get(x));
            let lane_from_id = t.destination_lane();
            let lane_from = lanes.get(lane_from_id);

            let lane_travers = inter.zip(lane_from).map(|(inter, lane_from)| {
                (
                    Traversable::new(
                        TraverseKind::Lane(lane_from_id),
                        lane_from.dir_from(inter.id),
                    ),
                    OrderedFloat(lane_from.points.length()),
                )
            });

            inter
                .into_iter()
                .flat_map(move |inter| {
                    inter.turns_from(lane_from_id).map(|(x, dir)| {
                        (
                            Traversable::new(TraverseKind::Turn(x), dir),
                            OrderedFloat(0.001),
                        )
                    })
                })
                .chain(lane_travers)
        };

        let has_arrived = |p: &Traversable| match p.kind {
            TraverseKind::Lane(id) => id == end,
            TraverseKind::Turn(_) => false,
        };

        pathfinding::directed::astar::astar(&start, successors, heuristic, has_arrived)
            .map(|(v, _)| v)
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

        let end_pos = inters.get(lanes.get(end)?.dst)?.pos;

        let can_drive = map.can_drive_between(start_lane, end);
        #[cfg(not(debug_assertions))]
        if !can_drive {
            return None;
        }

        let dummy = LaneID::null();

        const HEURISTIC_SPEED: f32 = LanePatternBuilder::new().speed_limit;

        let heuristic = |&p: &LaneID| {
            let pos = unwrap_ret!(
                inters.get(unwrap_ret!(lanes.get(p), OrderedFloat(f32::INFINITY)).dst),
                OrderedFloat(f32::INFINITY)
            )
            .pos;
            OrderedFloat(pos.distance(end_pos) * 1.2 / HEURISTIC_SPEED) // Inexact but (much) faster
        };

        let base_random = hash_u64((start_lane.data().as_ffi(), tick.0)) as u32;

        let successors = move |&p: &LaneID| {
            let lane = if p == dummy { start_lane } else { p };
            lane_turn_successors(map, lane).map(move |dst| {
                let mut cost = f32::INFINITY;

                if let Some(l) = lanes.get(dst) {
                    cost = l.points.length() / l.speed_limit;
                    cost += map
                        .lane_scoring_pilot()
                        .and_then(|pilot| pilot.wait_real_seconds(dst))
                        .unwrap_or_else(|| {
                            common::rand::randu(l.dist_from_bottom.to_bits() ^ base_random)
                        });
                }

                (dst, OrderedFloat(cost))
            })
        };

        let route =
            pathfinding::directed::astar::astar(&dummy, successors, heuristic, |p| *p == end);
        #[cfg(debug_assertions)]
        if !can_drive {
            assert!(
                route.is_none(),
                "reachability rejected a route from {start_lane:?} to {end:?}",
            );
        }
        let (v, _) = route?;

        let mut path = Vec::with_capacity(v.len() * 2);
        path.push(start);

        let mut last_id = start_lane;

        for lane in v.into_iter().skip(1) {
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
    pub(crate) fn can_drive_between(&self, start: LaneID, end: LaneID) -> bool {
        self.driving_reachability
            .get_or_init(|| super::reachability::DrivingReachability::build(self))
            .can_reach(start, end)
    }

    /// Labels the driving component of a lane. Lanes with the same label reach
    /// each other both ways. Labels last until the lane topology changes.
    pub(crate) fn driving_component(&self, lane: LaneID) -> Option<usize> {
        self.driving_reachability
            .get_or_init(|| super::reachability::DrivingReachability::build(self))
            .cyclic_component(lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{procgen, IntersectionID, MapProject, ProjectKind, RoadID, RoadSegmentKind};
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

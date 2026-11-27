use super::Itinerary;
use crate::map::{
    LaneID, LaneKind, Map, ProjectFilter, ProjectKind, RoadID, Traversable, TraverseDirection,
    TraverseKind, TurnID,
};
use geom::{Circle, PolyLine3, Segment3, Shape, Vec3};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use std::collections::{btree_set, BTreeMap, BTreeSet};

const WALK_ALGORITHM: u8 = 1;
const MAX_WALK_WORK: usize = 2_048;
const SIDEWALK_RADIUS: f32 = 24.0;
type Revision = (u64, u64);
type WalkKey = (TraverseKind, bool);

pub(super) enum WalkResult {
    Ready(ParkingWalkPlan),
    Deferred,
    Unavailable,
}

// Save the input and its debit. Restoring a cursor replays that debit against
// the current map, so a save cannot supply its own anchor or frontier.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct WalkProof {
    start: Vec3,
    end: Vec3,
    revision: Revision,
    algorithm: u8,
    spent: usize,
}

impl WalkProof {
    fn matches(self, map: &Map, start: Vec3, end: Vec3) -> bool {
        self.current(map) && self.start == start && self.end == end
    }

    fn current(self, map: &Map) -> bool {
        self.algorithm == WALK_ALGORITHM && self.revision == map.directed_route_revision()
    }

    fn valid(self) -> bool {
        self.start.is_finite() && self.end.is_finite() && self.spent <= MAX_WALK_WORK
    }

    fn replay(self, map: &Map) -> Option<(ParkingWalkSearch, WalkResult)> {
        let mut work = self.spent;
        let mut search = ParkingWalkSearch::new(map, self.start, self.end);
        let result = search.advance(map, &mut work);
        (work == 0).then_some((search, result))
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct ParkingWalkSearch {
    proof: WalkProof,
    #[serde(skip)]
    progress: WalkProgress,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ParkingWalkPlan {
    proof: WalkProof,
    #[serde(skip)]
    itinerary: Option<Itinerary>,
}

impl ParkingWalkPlan {
    pub(super) fn spent(&self) -> usize {
        self.proof.spent
    }

    pub(super) fn matches(&self, map: &Map, start: Vec3, end: Vec3) -> bool {
        self.proof.matches(map, start, end)
    }

    pub(super) fn validate_loaded(&mut self, map: &Map) -> bool {
        if !self.proof.valid() {
            return false;
        }
        if !self.proof.current(map) {
            return true;
        }
        let Some((_, WalkResult::Ready(plan))) = self.proof.replay(map) else {
            return false;
        };
        self.itinerary = plan.itinerary;
        true
    }

    pub(super) fn into_itinerary(self, map: &Map) -> Option<Itinerary> {
        self.proof.current(map).then_some(self.itinerary).flatten()
    }
}

impl ParkingWalkSearch {
    pub(super) fn spent(&self) -> usize {
        self.proof.spent
    }

    pub(super) fn new(map: &Map, start: Vec3, end: Vec3) -> Self {
        Self {
            proof: WalkProof {
                start,
                end,
                revision: map.directed_route_revision(),
                algorithm: WALK_ALGORITHM,
                spent: 0,
            },
            progress: WalkProgress::default(),
        }
    }

    pub(super) fn matches(&self, map: &Map, start: Vec3, end: Vec3) -> bool {
        self.proof.matches(map, start, end)
    }

    pub(super) fn validate_loaded(&mut self, map: &Map) -> bool {
        if !self.proof.valid() {
            return false;
        }
        if !self.proof.current(map) {
            return true;
        }
        let Some((search, WalkResult::Deferred)) = self.proof.replay(map) else {
            return false;
        };
        self.progress = search.progress;
        true
    }

    pub(super) fn advance(&mut self, map: &Map, work: &mut usize) -> WalkResult {
        if !self.proof.valid() {
            return WalkResult::Unavailable;
        }
        let before = *work;
        let result = self
            .progress
            .advance(map, self.proof.start, self.proof.end, work);
        self.proof.spent += before - *work;
        match result {
            Ok(Some(itinerary)) => WalkResult::Ready(ParkingWalkPlan {
                proof: self.proof,
                itinerary: Some(itinerary),
            }),
            Ok(None) => WalkResult::Unavailable,
            Err(()) => WalkResult::Deferred,
        }
    }
}

#[derive(Default)]
struct WalkProgress {
    start_anchor: Option<Anchor>,
    sidewalk: SidewalkSearch,
    end_anchor: Option<Anchor>,
    frontier: Option<WalkFrontier>,
    assembly: Option<WalkAssembly>,
}

impl WalkProgress {
    fn advance(
        &mut self,
        map: &Map,
        start: Vec3,
        end: Vec3,
        work: &mut usize,
    ) -> Result<Option<Itinerary>, ()> {
        if self.start_anchor.is_none() {
            let Some(anchor) = self.sidewalk.advance(map, start, work)? else {
                return Ok(None);
            };
            self.start_anchor = Some(anchor);
            self.sidewalk = SidewalkSearch::default();
        }
        let start_anchor = self.start_anchor.unwrap();
        if self.end_anchor.is_none() {
            let Some(anchor) = self.sidewalk.advance(map, end, work)? else {
                return Ok(None);
            };
            self.end_anchor = Some(anchor);
            self.sidewalk = SidewalkSearch::default();
        }
        let end_anchor = self.end_anchor.unwrap();
        if self.assembly.is_none() {
            let frontier = self
                .frontier
                .get_or_insert_with(|| WalkFrontier::new(start_on(start_anchor.lane)));
            let Some(route) = frontier.advance(map, end_anchor.lane, work)? else {
                return Ok(None);
            };
            self.assembly = Some(WalkAssembly::new(route, start_anchor, end_anchor));
        }
        let assembly = self.assembly.as_mut().unwrap();
        Ok(Some(assembly.advance(
            map,
            start,
            end,
            start_anchor,
            end_anchor,
            work,
        )?))
    }
}

#[derive(Clone, Copy)]
struct Anchor {
    lane: LaneID,
    projection: Projection,
    distance: f32,
}

#[derive(Clone, Copy)]
struct Projection {
    position: Vec3,
    segment_end: usize,
}

#[derive(Default)]
struct ProjectLine {
    index: usize,
    closest: Option<Projection>,
}

impl ProjectLine {
    fn advance(
        &mut self,
        points: &PolyLine3,
        reverse: bool,
        position: Vec3,
        work: &mut usize,
    ) -> Result<Projection, ()> {
        let len = points.len();
        if len == 1 {
            spend(work, 1)?;
            return Ok(Projection {
                position: points[0],
                segment_end: 0,
            });
        }
        while self.index < len - 1 {
            spend(work, 1)?;
            let segment = Segment3 {
                src: line_point(points, reverse, self.index),
                dst: line_point(points, reverse, self.index + 1),
            };
            // Match PolyLine3::project_segment, including its two-point and tie behavior.
            let projected = if len == 2 {
                segment.project(position)
            } else {
                segment.src
                    + (segment.dst - segment.src) * segment.flatten().project_t(position.xy())
            };
            self.index += 1;
            let distance = |p: Vec3| OrderedFloat((position - p).xy().mag());
            if self
                .closest
                .is_none_or(|best| distance(projected) < distance(best.position))
            {
                self.closest = Some(Projection {
                    position: projected,
                    segment_end: self.index,
                });
            }
        }
        Ok(self.closest.unwrap())
    }
}

fn line_point(points: &PolyLine3, reverse: bool, index: usize) -> Vec3 {
    points[if reverse {
        points.len() - 1 - index
    } else {
        index
    }]
}

#[derive(Default)]
struct WalkRoads {
    roads: Option<btree_set::IntoIter<RoadID>>,
}

impl WalkRoads {
    fn next(&mut self, map: &Map, position: Vec3, work: &mut usize) -> Result<Option<RoadID>, ()> {
        if self.roads.is_none() {
            let bounds = Circle {
                center: position.xy(),
                radius: SIDEWALK_RADIUS,
            }
            .bbox();
            let spatial = map.spatial_map();
            let mut remaining = *work;
            // The shared broad-phase API reserves the full local scan. Keep
            // the result in ID order across resumption and index rebuilds.
            spatial
                .reserve_construction_query(bounds, &mut remaining)
                .map_err(|_| ())?;
            let roads: BTreeSet<_> = spatial
                .construction_candidates(bounds, ProjectFilter::ROAD)
                .filter_map(|project| match project {
                    ProjectKind::Road(id) => Some(id),
                    _ => None,
                })
                .collect();
            self.roads = Some(roads.into_iter());
            *work = remaining;
        }
        Ok(self.roads.as_mut().unwrap().next())
    }
}

#[derive(Default)]
struct SidewalkSearch {
    roads: WalkRoads,
    road: Option<RoadID>,
    lane: usize,
    project: Option<ProjectLine>,
    best: Option<Anchor>,
}

impl SidewalkSearch {
    fn advance(
        &mut self,
        map: &Map,
        position: Vec3,
        work: &mut usize,
    ) -> Result<Option<Anchor>, ()> {
        loop {
            if self.road.is_none() {
                self.road = self.roads.next(map, position, work)?;
            }
            let Some(road) = self.road else {
                return Ok(self.best);
            };
            let Some((lane, kind)) = map.roads()[road].lanes_iter().nth(self.lane) else {
                self.road = None;
                self.lane = 0;
                continue;
            };
            if self.project.is_none() {
                spend(work, 1)?;
                if kind != LaneKind::Walking {
                    self.lane += 1;
                    continue;
                }
                self.project = Some(ProjectLine::default());
            }
            let projection = self.project.as_mut().unwrap().advance(
                &map.lanes()[lane].points,
                false,
                position,
                work,
            )?;
            let distance = projection.position.distance2(position);
            if distance <= SIDEWALK_RADIUS.powi(2)
                && self.best.is_none_or(|best| {
                    (OrderedFloat(distance), lane) < (OrderedFloat(best.distance), best.lane)
                })
            {
                self.best = Some(Anchor {
                    lane,
                    projection,
                    distance,
                });
            }
            self.lane += 1;
            self.project = None;
        }
    }
}

fn spend(work: &mut usize, amount: usize) -> Result<(), ()> {
    *work = work.checked_sub(amount).ok_or(())?;
    Ok(())
}

fn start_on(lane: LaneID) -> Traversable {
    Traversable::new(TraverseKind::Lane(lane), TraverseDirection::Forward)
}

fn key(traversal: Traversable) -> WalkKey {
    (traversal.kind, traversal.dir == TraverseDirection::Forward)
}

fn turn_from(turn: TurnID, lane: LaneID) -> Option<Traversable> {
    let direction = if turn.src == lane {
        TraverseDirection::Forward
    } else if turn.bidirectional && turn.dst == lane {
        TraverseDirection::Backward
    } else {
        return None;
    };
    Some(Traversable::new(TraverseKind::Turn(turn), direction))
}

struct WalkFrontier {
    nodes: Vec<WalkNode>,
    visited: BTreeMap<WalkKey, usize>,
    head: usize,
    after: Option<TurnID>,
    backtrack: Option<usize>,
    route: Vec<Traversable>,
}

struct WalkNode {
    traversal: Traversable,
    parent: Option<usize>,
    depth: usize,
}

impl WalkFrontier {
    fn new(start: Traversable) -> Self {
        Self {
            nodes: vec![WalkNode {
                traversal: start,
                parent: None,
                depth: 0,
            }],
            visited: BTreeMap::from([(key(start), 0)]),
            head: 0,
            after: None,
            backtrack: None,
            route: Vec::new(),
        }
    }

    fn advance(
        &mut self,
        map: &Map,
        end: LaneID,
        work: &mut usize,
    ) -> Result<Option<Vec<Traversable>>, ()> {
        while let Some(node) = self.nodes.get(self.head) {
            let current = node.traversal;
            if current.kind == TraverseKind::Lane(end) {
                if self.route.capacity() == 0 {
                    self.backtrack = Some(self.head);
                    self.route = Vec::with_capacity(node.depth + 1);
                }
                while let Some(index) = self.backtrack {
                    spend(work, 1)?;
                    let node = &self.nodes[index];
                    self.route.push(node.traversal);
                    self.backtrack = node.parent;
                }
                return Ok(Some(std::mem::take(&mut self.route)));
            }

            spend(work, 1)?;
            let lane = current.destination_lane();
            let Some(junction) = current
                .destination_intersection(map.lanes())
                .and_then(|id| map.get(id))
            else {
                return Ok(None);
            };
            if let Some(turn) = junction.next_turn(self.after) {
                self.after = Some(turn.id);
                if let Some(next) = turn_from(turn.id, lane) {
                    self.visit(next);
                }
            } else {
                self.visit(Traversable::new(
                    TraverseKind::Lane(lane),
                    map.lanes()[lane].dir_from(junction.id),
                ));
                self.head += 1;
                self.after = None;
            }
        }
        Ok(None)
    }

    fn visit(&mut self, traversal: Traversable) {
        if let std::collections::btree_map::Entry::Vacant(entry) =
            self.visited.entry(key(traversal))
        {
            entry.insert(self.nodes.len());
            self.nodes.push(WalkNode {
                traversal,
                parent: Some(self.head),
                depth: self.nodes[self.head].depth + 1,
            });
        }
    }
}

struct WalkAssembly {
    current: Traversable,
    route: Vec<Traversable>,
    project: ProjectLine,
    local: Option<LocalWalk>,
}

struct LocalWalk {
    points: Vec<Vec3>,
    next: usize,
    stop: usize,
    first: Vec3,
}

impl WalkAssembly {
    fn new(mut route: Vec<Traversable>, start: Anchor, end: Anchor) -> Self {
        let mut current = route.pop().unwrap();
        if let Some(&next) = route.last() {
            if next.kind == current.kind {
                current = route.pop().unwrap();
            }
        }
        let local = (start.lane == end.lane).then(|| {
            let next = start.projection.segment_end.max(end.projection.segment_end);
            let stop = start.projection.segment_end.min(end.projection.segment_end);
            LocalWalk {
                points: Vec::with_capacity(next - stop + 3),
                next,
                stop,
                first: start.projection.position,
            }
        });
        Self {
            current,
            route,
            project: ProjectLine::default(),
            local,
        }
    }

    fn advance(
        &mut self,
        map: &Map,
        start: Vec3,
        end: Vec3,
        start_anchor: Anchor,
        end_anchor: Anchor,
        work: &mut usize,
    ) -> Result<Itinerary, ()> {
        let points = &map.lanes()[start_anchor.lane].points;
        let reverse = self.current.dir == TraverseDirection::Backward;
        if self.local.is_none() {
            let projection = if reverse {
                self.project.advance(points, true, start, work)?
            } else {
                start_anchor.projection
            };
            let index = projection.segment_end.saturating_sub(1);
            let direction = if points.len() >= 2 {
                (line_point(points, reverse, index + 1) - line_point(points, reverse, index))
                    .try_normalize()
                    .unwrap_or(Vec3::X)
            } else {
                Vec3::X
            };
            self.local = Some(LocalWalk {
                points: Vec::with_capacity(points.len() - projection.segment_end + 1),
                next: points.len(),
                stop: projection.segment_end,
                first: projection.position + direction * 3.5,
            });
        }
        let local = self.local.as_mut().unwrap();
        if start_anchor.lane == end_anchor.lane {
            if local.points.is_empty() {
                spend(work, 1)?;
                local.points.push(end);
            }
            if local.points.len() == 1 {
                spend(work, 1)?;
                local.points.push(end_anchor.projection.position);
            }
        }
        while local.next > local.stop {
            spend(work, 1)?;
            local.next -= 1;
            local.points.push(line_point(points, reverse, local.next));
        }
        spend(work, 1)?;
        local.points.push(local.first);
        Ok(Itinerary::from_parking_walk(
            self.current,
            std::mem::take(&mut self.route),
            std::mem::take(&mut local.points),
            end,
        ))
    }
}

// The first tagged save stored unchecked walking state. Decode it within
// the original wire limits so the parking runtime can migrate its other fields.
#[derive(Deserialize)]
#[allow(dead_code)]
pub(super) struct LegacyWalkSearch(
    Vec3,
    Vec3,
    Revision,
    Option<LaneID>,
    LegacySidewalk,
    Option<LaneID>,
    Option<LegacyFrontier>,
);

type LegacySidewalk = (usize, bool, usize, usize, Option<(f32, LaneID)>);

#[derive(Deserialize)]
#[allow(dead_code)]
struct LegacyFrontier(
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_WALK_WORK>")]
    Vec<(Traversable, Option<usize>, usize)>,
    #[serde(
        deserialize_with = "crate::map::building_parking::bounded_parking_map::<_, _, _, MAX_WALK_WORK>"
    )]
    BTreeMap<WalkKey, usize>,
    usize,
    Option<TurnID>,
    usize,
);

#[derive(Deserialize)]
#[allow(dead_code)]
pub(super) struct LegacyWalkPlan(
    Vec3,
    Vec3,
    Revision,
    LaneID,
    LaneID,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_WALK_WORK>")]
    Vec<Traversable>,
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::PathKind;
    use crate::map_dynamic::ItineraryTarget;
    use crate::tests::pathpark_lifecycle::road_chain;
    use common::saveload::{Bincode, Encoder};

    #[test]
    fn pathpark_fix_imported_walk_requires_local_anchors() {
        let (ctx, roads, _) = road_chain(1);
        let map = ctx.g.map();
        let lane = map.roads()[roads[0]]
            .lanes_iter()
            .find(|&(_, kind)| kind == LaneKind::Walking)
            .unwrap()
            .0;
        let endpoint = map.lanes()[lane].points.middle() + Vec3::Y * 1_000.0;
        let plan = ParkingWalkPlan {
            proof: WalkProof {
                start: endpoint,
                end: endpoint,
                revision: map.directed_route_revision(),
                algorithm: WALK_ALGORITHM,
                spent: 300,
            },
            itinerary: None,
        };
        let mut restored: ParkingWalkPlan =
            Bincode::decode(&Bincode::encode(&plan).unwrap()).unwrap();
        assert!(
            !restored.validate_loaded(&map),
            "a live sidewalk can still be remote"
        );
    }

    #[test]
    fn pathpark_fix_imported_walk_checks_staged_sidewalk_progress() {
        let (ctx, _, _) = road_chain(1);
        let map = ctx.g.map();
        let endpoint = Vec3::new(800., 800., 0.);
        let mut search = ParkingWalkSearch::new(&map, endpoint, endpoint);
        search.proof.spent = MAX_WALK_WORK;
        let mut restored: ParkingWalkSearch =
            Bincode::decode(&Bincode::encode(&search).unwrap()).unwrap();
        assert!(
            !restored.validate_loaded(&map),
            "replay must account for the claimed work and stage"
        );
    }

    #[test]
    fn pathpark_pedestrian_candidate_geometry_is_charged_on_a_miss() {
        let points = PolyLine3::new((0..401).map(|i| Vec3::new(i as f32, 100., 0.)).collect());
        let endpoint = Vec3::new(180.5, 0., 0.);
        let mut project = ProjectLine::default();
        for segments in [1, 127, 271] {
            let mut work = segments;
            assert!(project
                .advance(&points, false, endpoint, &mut work)
                .is_err());
            assert_eq!(work, 0);
        }
        assert_eq!(project.index, 399);
        let mut final_segment = 1;
        let projected = project
            .advance(&points, false, endpoint, &mut final_segment)
            .unwrap();
        assert_eq!(final_segment, 0);
        assert_eq!(projected.position, points.project(endpoint));
        assert!(projected.position.distance2(endpoint) > SIDEWALK_RADIUS.powi(2));
    }

    #[test]
    fn pathpark_pedestrian_frontier_resumes_after_save() {
        let (ctx, roads, _) = road_chain(2);
        let map = ctx.g.map();
        let sidewalks = |road| {
            map.roads()[road]
                .lanes_iter()
                .filter(|&(_, kind)| kind == LaneKind::Walking)
                .map(|(id, _)| map.lanes()[id].points.middle())
        };
        let (start, end, expected) = sidewalks(roads[0])
            .find_map(|start| {
                sidewalks(roads[1]).find_map(|end| {
                    let mut work = MAX_WALK_WORK;
                    let WalkResult::Ready(plan) =
                        ParkingWalkSearch::new(&map, start, end).advance(&map, &mut work)
                    else {
                        return None;
                    };
                    Some((start, end, plan))
                })
            })
            .expect("the connected roads must share a sidewalk route");

        let mut work = MAX_WALK_WORK;
        let start_anchor = SidewalkSearch::default()
            .advance(&map, start, &mut work)
            .unwrap()
            .unwrap();
        let end_anchor = SidewalkSearch::default()
            .advance(&map, end, &mut work)
            .unwrap()
            .unwrap();

        let mut frontier = WalkFrontier::new(start_on(start_anchor.lane));
        let mut route_work = MAX_WALK_WORK;
        let route = frontier
            .advance(&map, end_anchor.lane, &mut route_work)
            .unwrap()
            .unwrap();
        let stock = Itinerary::route_from_lanes(
            start,
            start_anchor.lane,
            end_anchor.lane,
            end,
            ItineraryTarget::Position(end),
            &map,
            PathKind::Pedestrian,
            |_| Some(route.into_iter().rev().collect()),
        )
        .unwrap();
        assert_eq!(
            Bincode::encode(&stock).unwrap(),
            Bincode::encode(expected.itinerary.as_ref().unwrap()).unwrap()
        );

        let mut search = ParkingWalkSearch::new(&map, start, end);
        let mut first_slice = MAX_WALK_WORK - work + 3;
        assert!(matches!(
            search.advance(&map, &mut first_slice),
            WalkResult::Deferred
        ));
        assert_eq!(first_slice, 0);
        assert!(search.progress.frontier.is_some());
        let snapshot = Bincode::encode(&search).unwrap();
        let mut restored: ParkingWalkSearch = Bincode::decode(&snapshot).unwrap();
        assert!(restored.validate_loaded(&map));
        // Run the frontier, backtrack and itinerary assembly one item at a time.
        let plan = loop {
            let mut one = 1;
            match restored.advance(&map, &mut one) {
                WalkResult::Deferred => assert_eq!(one, 0),
                WalkResult::Ready(plan) => break plan,
                WalkResult::Unavailable => panic!("the restored walk must reach the goal"),
            }
        };
        assert_eq!(
            Bincode::encode(&plan).unwrap(),
            Bincode::encode(&expected).unwrap()
        );
        let mut loaded: ParkingWalkPlan =
            Bincode::decode(&Bincode::encode(&plan).unwrap()).unwrap();
        assert!(loaded.validate_loaded(&map));
        assert_eq!(
            Bincode::encode(&plan.itinerary).unwrap(),
            Bincode::encode(&loaded.itinerary).unwrap()
        );
    }
}

use super::{Itinerary, ItineraryTarget};
use crate::map::building_parking::bounded_parking_map;
use crate::map::{
    LaneID, LaneKind, Map, PathKind, Traversable, TraverseDirection, TraverseKind, TurnID,
};
use geom::Vec3;
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_WALK_WORK: usize = 2_048;
const SIDEWALK_RADIUS: f32 = 24.0;
type Revision = (u64, u64);
type WalkKey = (TraverseKind, bool);

pub(super) enum WalkResult {
    Ready(ParkingWalkPlan),
    Deferred,
    Unavailable,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ParkingWalkSearch {
    start: Vec3,
    end: Vec3,
    revision: Revision,
    start_lane: Option<LaneID>,
    sidewalk: SidewalkSearch,
    end_lane: Option<LaneID>,
    path: Option<WalkFrontier>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ParkingWalkPlan {
    start: Vec3,
    end: Vec3,
    revision: Revision,
    start_lane: LaneID,
    end_lane: LaneID,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_WALK_WORK>")]
    path: Vec<Traversable>,
}

impl ParkingWalkPlan {
    pub(super) fn matches(&self, map: &Map, start: Vec3, end: Vec3) -> bool {
        self.revision == map.directed_route_revision() && self.start == start && self.end == end
    }

    pub(super) fn validate_loaded(&self, map: &Map) -> bool {
        self.start.is_finite()
            && self.end.is_finite()
            && (self.revision != map.directed_route_revision()
                || (walk_lane(map, self.start_lane)
                    && walk_lane(map, self.end_lane)
                    && self.path.first() == Some(&start_on(self.start_lane))
                    && self
                        .path
                        .last()
                        .is_some_and(|last| last.kind == TraverseKind::Lane(self.end_lane))
                    && self
                        .path
                        .windows(2)
                        .all(|pair| valid_step(map, pair[0], pair[1]))))
    }

    pub(super) fn into_itinerary(self, map: &Map) -> Option<Itinerary> {
        Itinerary::route_from_lanes(
            self.start,
            self.start_lane,
            self.end_lane,
            self.end,
            ItineraryTarget::Position(self.end),
            map,
            PathKind::Pedestrian,
            |_| Some(self.path),
        )
    }
}

impl ParkingWalkSearch {
    pub(super) fn new(map: &Map, start: Vec3, end: Vec3) -> Self {
        Self {
            start,
            end,
            revision: map.directed_route_revision(),
            start_lane: None,
            sidewalk: SidewalkSearch::default(),
            end_lane: None,
            path: None,
        }
    }

    pub(super) fn matches(&self, map: &Map, start: Vec3, end: Vec3) -> bool {
        self.revision == map.directed_route_revision() && self.start == start && self.end == end
    }

    pub(super) fn validate_loaded(&self, map: &Map) -> bool {
        self.start.is_finite()
            && self.end.is_finite()
            && (self.revision != map.directed_route_revision()
                || (self.start_lane.is_none_or(|id| walk_lane(map, id))
                    && self
                        .end_lane
                        .is_none_or(|id| self.start_lane.is_some() && walk_lane(map, id))
                    && self.sidewalk.validate_loaded(map)
                    && self.path.as_ref().is_none_or(|path| {
                        self.start_lane
                            .zip(self.end_lane)
                            .is_some_and(|(start, _)| path.validate_loaded(map, start_on(start)))
                    })))
    }

    pub(super) fn advance(&mut self, map: &Map, work: &mut usize) -> WalkResult {
        if *work == 0 {
            return WalkResult::Deferred;
        }
        if self.start_lane.is_none() {
            match self.sidewalk.advance(map, self.start, work) {
                Ok(Some(lane)) => {
                    self.start_lane = Some(lane);
                    self.sidewalk = SidewalkSearch::default();
                }
                Ok(None) => return WalkResult::Unavailable,
                Err(()) => return WalkResult::Deferred,
            }
        }
        if self.end_lane.is_none() {
            match self.sidewalk.advance(map, self.end, work) {
                Ok(Some(lane)) => {
                    self.end_lane = Some(lane);
                    self.sidewalk = SidewalkSearch::default();
                }
                Ok(None) => return WalkResult::Unavailable,
                Err(()) => return WalkResult::Deferred,
            }
        }
        let start_lane = self.start_lane.unwrap();
        let end_lane = self.end_lane.unwrap();
        let path = self
            .path
            .get_or_insert_with(|| WalkFrontier::new(start_on(start_lane)));
        // Reserve the endpoint geometry before the caller constructs the itinerary.
        let geometry = map.lanes()[start_lane].points.len() + map.lanes()[end_lane].points.len();
        match path.advance(map, end_lane, geometry, work) {
            Ok(Some(path)) => WalkResult::Ready(ParkingWalkPlan {
                start: self.start,
                end: self.end,
                revision: self.revision,
                start_lane,
                end_lane,
                path,
            }),
            Ok(None) => WalkResult::Unavailable,
            Err(()) => WalkResult::Deferred,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SidewalkSearch {
    road: usize,
    road_entered: bool,
    lane: usize,
    lane_credit: usize,
    best: Option<(f32, LaneID)>,
}

impl SidewalkSearch {
    fn validate_loaded(&self, map: &Map) -> bool {
        self.road <= 128
            && self.lane <= MAX_WALK_WORK
            && self.lane_credit <= MAX_WALK_WORK
            && self.best.is_none_or(|(distance, lane)| {
                (0.0..=SIDEWALK_RADIUS.powi(2)).contains(&distance) && walk_lane(map, lane)
            })
    }

    fn advance(
        &mut self,
        map: &Map,
        position: Vec3,
        work: &mut usize,
    ) -> Result<Option<LaneID>, ()> {
        if !position.is_finite() {
            return Ok(None);
        }
        let roads = map
            .spatial_map()
            .parking_roads(position.xy(), SIDEWALK_RADIUS);
        for road in roads.skip(self.road) {
            if !self.road_entered {
                spend(work, 1)?;
                self.road_entered = true;
            }
            for (lane, kind) in map.roads()[road].lanes_iter().skip(self.lane) {
                // Reserve the full geometry before projection. The debit can span
                // slices, including for candidates that lose the distance comparison.
                let points = &map.lanes()[lane].points;
                let geometry = if kind == LaneKind::Walking {
                    points.len()
                } else {
                    0
                };
                accrue(work, &mut self.lane_credit, 1 + geometry)?;
                self.lane_credit = 0;
                self.lane += 1;
                if kind == LaneKind::Walking {
                    let distance = points.project_dist2(position);
                    if distance <= SIDEWALK_RADIUS.powi(2)
                        && self.best.is_none_or(|(best, id)| {
                            (OrderedFloat(distance), lane) < (OrderedFloat(best), id)
                        })
                    {
                        self.best = Some((distance, lane));
                    }
                }
            }
            self.lane = 0;
            self.road_entered = false;
            self.road += 1;
        }
        Ok(self.best.map(|(_, lane)| lane))
    }
}

fn spend(work: &mut usize, amount: usize) -> Result<(), ()> {
    *work = work.checked_sub(amount).ok_or(())?;
    Ok(())
}

fn accrue(work: &mut usize, credit: &mut usize, amount: usize) -> Result<(), ()> {
    let debit = amount.saturating_sub(*credit).min(*work);
    *work -= debit;
    *credit += debit;
    (*credit == amount).then_some(()).ok_or(())
}

fn start_on(lane: LaneID) -> Traversable {
    Traversable::new(TraverseKind::Lane(lane), TraverseDirection::Forward)
}

fn key(traversal: Traversable) -> WalkKey {
    (traversal.kind, traversal.dir == TraverseDirection::Forward)
}

fn walk_lane(map: &Map, id: LaneID) -> bool {
    map.get(id)
        .is_some_and(|lane| lane.kind == LaneKind::Walking)
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

fn valid_step(map: &Map, previous: Traversable, next: Traversable) -> bool {
    let Some(junction) = previous
        .destination_intersection(map.lanes())
        .and_then(|id| map.get(id))
    else {
        return false;
    };
    let lane = previous.destination_lane();
    if !walk_lane(map, next.destination_lane()) {
        return false;
    }
    match next.kind {
        TraverseKind::Turn(turn) => {
            turn.parent == junction.id
                && junction.find_turn(turn).is_some()
                && turn_from(turn, lane) == Some(next)
        }
        TraverseKind::Lane(id) => id == lane && map.lanes()[id].dir_from(junction.id) == next.dir,
    }
}

#[derive(Serialize, Deserialize)]
struct WalkFrontier {
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_WALK_WORK>")]
    nodes: Vec<WalkNode>,
    #[serde(deserialize_with = "bounded_parking_map::<_, _, _, MAX_WALK_WORK>")]
    visited: BTreeMap<WalkKey, usize>,
    head: usize,
    // The last inspected turn at the head, including unrelated turns.
    after: Option<TurnID>,
    goal_credit: usize,
}

#[derive(Serialize, Deserialize)]
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
            goal_credit: 0,
        }
    }

    fn validate_loaded(&self, map: &Map, start: Traversable) -> bool {
        self.head < self.nodes.len()
            && self.goal_credit <= MAX_WALK_WORK
            && self.visited.len() == self.nodes.len()
            && self.nodes.iter().enumerate().all(|(index, node)| {
                self.visited.get(&key(node.traversal)) == Some(&index)
                    && match node.parent {
                        Some(parent) if parent < index => {
                            node.depth == self.nodes[parent].depth + 1
                                && valid_step(map, self.nodes[parent].traversal, node.traversal)
                        }
                        None => index == 0 && node.traversal == start && node.depth == 0,
                        _ => false,
                    }
            })
            && self.after.is_none_or(|turn| {
                self.nodes[self.head]
                    .traversal
                    .destination_intersection(map.lanes())
                    == Some(turn.parent)
                    && map
                        .get(turn.parent)
                        .is_some_and(|junction| junction.find_turn(turn).is_some())
            })
    }

    fn advance(
        &mut self,
        map: &Map,
        end: LaneID,
        geometry: usize,
        work: &mut usize,
    ) -> Result<Option<Vec<Traversable>>, ()> {
        while let Some(node) = self.nodes.get(self.head) {
            let current = node.traversal;
            if current.kind == TraverseKind::Lane(end) {
                accrue(work, &mut self.goal_credit, node.depth + 1 + geometry)?;
                let mut path = Vec::with_capacity(node.depth + 1);
                let mut index = Some(self.head);
                while let Some(at) = index {
                    path.push(self.nodes[at].traversal);
                    index = self.nodes[at].parent;
                }
                path.reverse();
                return Ok(Some(path));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::pathpark_lifecycle::road_chain;
    use common::saveload::{Bincode, Encoder};

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
        SidewalkSearch::default()
            .advance(&map, start, &mut work)
            .unwrap()
            .unwrap();
        SidewalkSearch::default()
            .advance(&map, end, &mut work)
            .unwrap()
            .unwrap();
        let geometry = MAX_WALK_WORK - work;
        let mut search = ParkingWalkSearch::new(&map, start, end);
        let mut first_slice = geometry + 3;
        assert!(matches!(
            search.advance(&map, &mut first_slice),
            WalkResult::Deferred
        ));
        assert_eq!(first_slice, 0);
        assert!(search.path.is_some());
        let snapshot = Bincode::encode(&search).unwrap();
        let mut restored: ParkingWalkSearch = Bincode::decode(&snapshot).unwrap();
        assert!(restored.validate_loaded(&map));
        let WalkResult::Ready(plan) = restored.advance(&map, &mut work) else {
            panic!("the restored walking cursor must reach the goal");
        };
        assert_eq!(
            Bincode::encode(&plan).unwrap(),
            Bincode::encode(&expected).unwrap()
        );
        assert!(plan.validate_loaded(&map));
    }
}

use crate::map::{
    LaneKind, Map, ProjectFilter, ProjectKind, Traversable, TraverseDirection, TraverseKind, Turn,
    TurnKind,
};
use geom::{Vec2, Vec3, AABB};
use ordered_float::OrderedFloat;
use std::collections::BTreeSet;

const SEARCH_RADIUS: f32 = 1.25;
const DECK_TOLERANCE: f32 = 0.65;
const PEDESTRIAN_RADIUS: f32 = 0.3;
const LOCAL_WORK: usize = 1_024;

pub(super) enum PedestrianClearance {
    Clear,
    Hold,
    Crosswalk {
        traversal: Traversable,
        projection: Vec3,
        sidewalk: Vec3,
    },
}

/// A stranded walker may leave on a crossing they already occupy. Preserve
/// their obstruction when no bounded, same-deck path can be established.
pub(super) fn pedestrian_clearance(map: &Map, position: Vec3) -> PedestrianClearance {
    find_clearance(map, position).unwrap_or(PedestrianClearance::Hold)
}

fn find_clearance(map: &Map, position: Vec3) -> Option<PedestrianClearance> {
    if !position.is_finite() {
        return None;
    }
    let spatial = map.spatial_map();
    let center = position.xy();
    let extent = Vec2::splat(SEARCH_RADIUS);
    let bounds = AABB::new_ll_ur(center - extent, center + extent);
    let mut work = LOCAL_WORK;
    spatial.reserve_construction_query(bounds, &mut work).ok()?;

    let mut intersections = BTreeSet::new();
    let mut vehicle_path = false;
    for candidate in spatial.query_around(
        center,
        SEARCH_RADIUS,
        ProjectFilter::INTER | ProjectFilter::ROAD,
    ) {
        work = work.checked_sub(1)?;
        match candidate {
            ProjectKind::Intersection(id) => {
                intersections.insert(id);
            }
            ProjectKind::Road(id) => {
                let road = map.get(id)?;
                intersections.extend([road.src, road.dst]);
                for (lane, kind) in road.lanes_iter() {
                    work = work.checked_sub(1)?;
                    if !kind.vehicles() && kind != LaneKind::Rail {
                        continue;
                    }
                    let lane = map.get(lane)?;
                    work = work.checked_sub(lane.points.len())?;
                    vehicle_path |= occupies_path(&lane.points, kind.width() * 0.5, position);
                }
            }
            _ => {}
        }
    }

    let mut best = None;
    let mut vehicle_turns = Vec::new();
    for id in intersections {
        work = work.checked_sub(1)?;
        let junction = map.get(id)?;
        // Adjacent roads supply their endpoints as well, including endpoints
        // where the crossing lies just outside the junction's spatial circle.
        if !junction
            .pos
            .xy()
            .is_close(center, junction.radius + 2.0 * SEARCH_RADIUS)
        {
            continue;
        }
        for turn in junction.turns() {
            work = work.checked_sub(1)?;
            if turn.kind != TurnKind::Crosswalk {
                if !vehicle_path && matches!(turn.kind, TurnKind::Driving | TurnKind::Rail) {
                    vehicle_turns.push(turn);
                }
                continue;
            }
            work = work.checked_sub(turn.points.len())?;
            if let Some((projection, sidewalk, dir)) = crosswalk_exit(map, turn, position) {
                let key = (
                    OrderedFloat(position.distance2(projection)),
                    OrderedFloat(projection.distance2(sidewalk)),
                    turn.id,
                );
                if best.as_ref().is_none_or(|(current, _)| key < *current) {
                    best = Some((
                        key,
                        PedestrianClearance::Crosswalk {
                            traversal: Traversable::new(TraverseKind::Turn(turn.id), dir),
                            projection,
                            sidewalk,
                        },
                    ));
                }
            }
        }
    }
    // A verified crossing already gives this walker a physical exit.
    // Only inspect the vehicle sweeps when deciding whether an unmarked
    // position can safely release its collider.
    if let Some((_, crossing)) = best {
        return Some(crossing);
    }
    if vehicle_path {
        return Some(PedestrianClearance::Hold);
    }
    for turn in vehicle_turns {
        work = work.checked_sub(turn.points.len())?;
        if occupies_path(&turn.points, 2.0, position) {
            return Some(PedestrianClearance::Hold);
        }
    }
    Some(PedestrianClearance::Clear)
}

fn occupies_path(points: &geom::PolyLine3, half_width: f32, position: Vec3) -> bool {
    let (nearest, _) = points.project_segment_2d(position.xy());
    (position.z - nearest.z).abs() <= DECK_TOLERANCE
        && nearest
            .xy()
            .is_close(position.xy(), half_width + PEDESTRIAN_RADIUS)
}

fn crosswalk_exit(
    map: &Map,
    turn: &Turn,
    position: Vec3,
) -> Option<(Vec3, Vec3, TraverseDirection)> {
    if turn.kind != TurnKind::Crosswalk
        || turn.points.len() != 2
        || !turn.id.bidirectional
        || [turn.id.src, turn.id.dst].into_iter().any(|id| {
            map.get(id)
                .is_none_or(|lane| lane.kind != LaneKind::Walking)
        })
    {
        return None;
    }
    let first = turn.points.first();
    let last = turn.points.last();
    let (projection, _) = turn.points.project_segment_2d(position.xy());
    if (position.z - projection.z).abs() > DECK_TOLERANCE
        || !position.xy().is_close(projection.xy(), SEARCH_RADIUS)
        || projection.is_close(first, PEDESTRIAN_RADIUS)
        || projection.is_close(last, PEDESTRIAN_RADIUS)
    {
        return None;
    }
    if first.distance2(projection) < last.distance2(projection) {
        Some((projection, first, TraverseDirection::Backward))
    } else {
        Some((projection, last, TraverseDirection::Forward))
    }
}

pub(super) fn crosswalk_path_still_valid(
    map: &Map,
    crossing: Traversable,
    position: Vec3,
    path: &[Vec3],
) -> bool {
    let TraverseKind::Turn(id) = crossing.kind else {
        return false;
    };
    let Some(turn) = map.get(id.parent).and_then(|inter| inter.find_turn(id)) else {
        return false;
    };
    if turn.kind != TurnKind::Crosswalk || turn.points.len() != 2 || !id.bidirectional {
        return false;
    }
    let exit = match crossing.dir {
        TraverseDirection::Forward => turn.points.last(),
        TraverseDirection::Backward => turn.points.first(),
    };
    path.first() == Some(&exit)
        && occupies_path(&turn.points, SEARCH_RADIUS, position)
        && path
            .iter()
            .all(|&point| turn.points.project_dist2(point) < 0.01)
}

#[cfg(test)]
#[test]
fn p7_life_dense_junction_can_clear_a_cancelled_walker() {
    use crate::map::{LanePatternBuilder, MapProject};
    use crate::tests::TestCtx;
    use geom::vec3;

    let ctx = TestCtx::new();
    let center = vec3(420.0, 420.0, 10.0);
    let mut map = ctx.g.map_mut();
    let pattern = LanePatternBuilder::new().n_lanes(3).build();
    let (_, first) = map
        .make_connection(
            MapProject::ground(center),
            MapProject::ground(center + vec3(275.0, 0.0, 0.0)),
            None,
            &pattern,
        )
        .unwrap();
    let id = map.roads()[first].src;
    for spoke in 1..6 {
        let angle = spoke as f32 * std::f32::consts::TAU / 6.0;
        let (sin, cos) = angle.sin_cos();
        map.make_connection(
            MapProject {
                kind: ProjectKind::Intersection(id),
                pos: center,
            },
            MapProject::ground(center + vec3(275.0 * cos, 275.0 * sin, 0.0)),
            None,
            &pattern,
        )
        .unwrap();
    }
    let junction = map.get(id).unwrap();
    let full_turn_work: usize = junction.turns().map(|turn| 1 + turn.points.len()).sum();
    assert!(full_turn_work > LOCAL_WORK);
    let crosswalk = junction
        .turns()
        .find(|turn| turn.kind == TurnKind::Crosswalk)
        .unwrap();
    let position = crosswalk.points.middle();
    assert!(matches!(
        pedestrian_clearance(&map, position),
        PedestrianClearance::Crosswalk { traversal, .. }
            if traversal.kind == TraverseKind::Turn(crosswalk.id)
    ));
}

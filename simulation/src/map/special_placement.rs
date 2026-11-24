use geom::{Circle, Intersect, Polygon, Radians, Shape, Vec2, Vec3, OBB};
use ordered_float::OrderedFloat;
use prototypes::Size2D;
use std::collections::BTreeSet;

use crate::world_command::Rejection;

use super::{
    Building, Intersection, IntersectionID, Map, ProjectFilter, ProjectKind, Road, RoadID, Turn,
};

const CURB_SETBACK_M: f32 = 0.25;
const FALLBACK_DISTANCE_RATIO: f32 = 1.5;
const FALLBACK_EXTRA_DISTANCE_M: f32 = 10.0;

#[derive(Debug)]
pub struct SpecialBuildArgs {
    pub obb: OBB,
    pub connected_road: RoadID,
    pub height: f32,
    pub using_fallback: bool,
}

#[derive(Debug, PartialEq)]
pub enum PlacementError {
    NoRoad,
    NoSidewalk,
    InsufficientFrontage { required_m: f32, available_m: f32 },
    IntersectionOverlap,
    Occupied,
    InvalidTerrain,
    CommandRejected(Rejection),
}

#[derive(Debug)]
pub struct PlacementFailure {
    pub error: PlacementError,
    pub attempted: Option<PlacementPose>,
}

#[derive(Debug, Clone, Copy)]
pub struct PlacementPose {
    pub obb: OBB,
    pub height: Option<f32>,
}

impl From<PlacementError> for PlacementFailure {
    fn from(error: PlacementError) -> Self {
        Self {
            error,
            attempted: None,
        }
    }
}

impl PlacementPose {
    fn failure(self, error: PlacementError) -> PlacementFailure {
        PlacementFailure {
            error,
            attempted: Some(self),
        }
    }
}

/// Find a clear pose and validate its command payload. Return the closest road's failure
/// when neither it nor a nearby fallback can be used.
pub fn snap_special_building<T>(
    map: &Map,
    cursor: Vec3,
    size: Size2D,
    mut validate: impl FnMut(&SpecialBuildArgs) -> Result<T, Rejection>,
) -> Result<(SpecialBuildArgs, T), PlacementFailure> {
    let reach = size.diag() * 0.5;
    if !cursor.is_finite() || !reach.is_finite() || size.w <= 0.0 || size.h <= 0.0 {
        return Err(PlacementError::InvalidTerrain.into());
    }

    let cursor = cursor.xy();
    let mut candidates: Vec<_> = map
        .spatial_map()
        .query_around(cursor, reach, ProjectFilter::ROAD)
        .filter_map(|hit| match hit {
            ProjectKind::Road(id) => map.get(id),
            _ => None,
        })
        .filter_map(|road| {
            let distance = road.points().project_2d(cursor).xy().distance(cursor);
            let max_distance = reach + road.width * 0.5;
            (distance <= max_distance).then_some((OrderedFloat(distance), road.id, road))
        })
        .collect();
    candidates.sort_unstable_by_key(|&(distance, id, _)| (distance, id));

    let Some(&(OrderedFloat(nearest_distance), nearest_id, _)) = candidates.first() else {
        return Err(PlacementError::NoRoad.into());
    };
    let fallback_limit = (nearest_distance * FALLBACK_DISTANCE_RATIO)
        .min(nearest_distance + FALLBACK_EXTRA_DISTANCE_M);

    let mut nearest_error = None;
    for (OrderedFloat(distance), id, road) in candidates {
        if distance > fallback_limit {
            break;
        }
        let placement = snap_to_road(map, cursor, size, road).and_then(|mut args| {
            args.using_fallback = id != nearest_id;
            match validate(&args) {
                Ok(payload) => Ok((args, payload)),
                Err(reason) => Err(PlacementPose {
                    obb: args.obb,
                    height: Some(args.height),
                }
                .failure(PlacementError::CommandRejected(reason))),
            }
        });
        match placement {
            Ok(placement) => return Ok(placement),
            Err(error) => {
                nearest_error.get_or_insert(error);
            }
        }
    }
    Err(nearest_error.unwrap_or_else(|| PlacementError::NoRoad.into()))
}

fn snap_to_road(
    map: &Map,
    cursor: Vec2,
    size: Size2D,
    road: &Road,
) -> Result<SpecialBuildArgs, PlacementFailure> {
    // Keep live junction interfaces. Use the full centerline at dead ends and check
    // the cap for overlap just like any other curb.
    let dead_end = |id: IntersectionID| map.get(id).is_some_and(|inter| inter.roads.len() == 1);
    let points = road.points();
    let usable = match (dead_end(road.src), dead_end(road.dst)) {
        (true, true) => points.flatten(),
        (true, false) => points.cut(0.0, road.interface_from(road.dst)).flatten(),
        (false, true) => points.cut(road.interface_from(road.src), 0.0).flatten(),
        (false, false) => road.interfaced_points().flatten(),
    };
    let available_m = usable.length();
    let frontage_error = PlacementError::InsufficientFrontage {
        required_m: size.h,
        available_m,
    };
    // A single-point projection returns an end index of zero.
    if usable.n_points() < 2 || available_m <= 0.0 || !available_m.is_finite() {
        return Err(frontage_error.into());
    }

    let (projected, segment_end, tangent) = usable.project_segment_dir(cursor);
    let forward_side = (cursor - projected).dot(tangent.perpendicular()) > 0.0;
    let sidewalks = road.sidewalks(road.src);
    let (sidewalk, side_sign) = if forward_side {
        (sidewalks.outgoing, 1.0)
    } else {
        (sidewalks.incoming, -1.0)
    };

    let before_segment: f32 = usable
        .segments()
        .take(segment_end - 1)
        .map(|segment| segment.src.distance(segment.dst))
        .sum();
    let along = before_segment + usable[segment_end - 1].distance(projected);
    let half_frontage = size.h * 0.5;
    let along = if size.h <= available_m {
        along.clamp(half_frontage, available_m - half_frontage)
    } else {
        available_m * 0.5
    };
    let (curb_center, tangent) = usable.point_dir_along(along);
    let normal = tangent.perpendicular() * side_sign;
    let setback = size.w * 0.5 + road.width * 0.5 + CURB_SETBACK_M;
    let obb = OBB::new(curb_center + normal * setback, normal, size.w, size.h);

    let terrain_height = |p| {
        map.environment
            .height(p)
            .filter(|height| height.is_finite())
    };
    let pose = PlacementPose {
        obb,
        height: terrain_height(obb.center()),
    };

    if size.h > available_m {
        return Err(pose.failure(frontage_error));
    }
    if sidewalk.is_none() {
        return Err(pose.failure(PlacementError::NoSidewalk));
    }
    if !Building::dry_footprint(map.environment(), &obb, None) {
        return Err(pose.failure(PlacementError::CommandRejected(Rejection::WetFootprint)));
    }
    if let Some(error) = placement_overlap(map, road, obb) {
        return Err(pose.failure(error));
    }

    let height = pose
        .height
        .filter(|_| obb.corners.iter().all(|&p| terrain_height(p).is_some()))
        .ok_or_else(|| pose.failure(PlacementError::InvalidTerrain))?;
    Ok(SpecialBuildArgs {
        obb,
        connected_road: road.id,
        height,
        using_fallback: false,
    })
}

fn placement_overlap(map: &Map, selected: &Road, obb: OBB) -> Option<PlacementError> {
    let footprint = Polygon::from(obb.corners.as_slice());
    let mut junctions = BTreeSet::from([selected.src, selected.dst]);
    let mut road_overlap = false;
    let filter = ProjectFilter::ROAD | ProjectFilter::INTER;
    for hit in map.spatial_map().query(obb.bbox(), filter) {
        match hit {
            ProjectKind::Road(id) => {
                if let Some(road) = map.get(id) {
                    junctions.extend([road.src, road.dst]);
                    // Test the curb by segment-to-polygon distance. An expanded OBB
                    // overestimates the road at its corners.
                    road_overlap |= road.boldline().intersects(&footprint);
                }
            }
            ProjectKind::Intersection(id) => {
                junctions.insert(id);
            }
            _ => (),
        }
    }

    if junctions.into_iter().any(|id| {
        map.get(id)
            .is_some_and(|inter| intersection_overlaps(map, inter, &obb))
    }) {
        return Some(PlacementError::IntersectionOverlap);
    }
    (road_overlap || map.building_overlaps(obb)).then_some(PlacementError::Occupied)
}

fn intersection_overlaps(map: &Map, inter: &Intersection, obb: &OBB) -> bool {
    if inter.roads.is_empty() {
        return obb.intersects(&Circle {
            center: inter.pos.xy(),
            radius: 5.5,
        });
    }

    // Bound the junction by the full-width mouths of its roads and the same corner
    // curves used to join them in the map mesh.
    let mouth = |id: RoadID| {
        let road = map.get(id)?;
        let points = road.interfaced_points();
        let (at, dir) = if road.src == inter.id {
            (points.first(), points.first_dir()?)
        } else {
            (points.last(), -points.last_dir()?)
        };
        let dir = dir.xy().try_normalize()?;
        let offset = dir.perpendicular() * (road.width * 0.5);
        Some((at.xy() - offset, at.xy() + offset, dir))
    };
    let mut curb = Polygon::default();
    let center = inter.pos.xy();
    let roundabout = inter
        .turn_policy
        .roundabout
        .filter(|_| inter.is_roundabout());
    for (&road, &next) in inter.roads.iter().zip(inter.roads.iter().cycle().skip(1)) {
        let (Some((left, _, dir)), Some((_, right, next_dir))) = (mouth(road), mouth(next)) else {
            continue;
        };
        if let Some(roundabout) = roundabout {
            let angle = (left - center).angle(right - center).abs();
            if angle >= Radians::from_deg(21.0).0 {
                curb.extend(Turn::gen_roundabout(
                    left.z0(),
                    right.z0(),
                    -dir,
                    next_dir,
                    roundabout.radius + 3.0,
                    center,
                ));
                continue;
            }
        }
        curb.extend(Turn::spline(left, right, -dir, next_dir).smart_points(1.0, 0.0, 1.0));
    }
    curb.len() >= 3 && obb.intersects(&curb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Environment, LanePatternBuilder, MapProject};
    use geom::vec2;

    #[test]
    fn w2b_water_special_preview_checks_footprint_before_callback() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        map.make_connection(
            MapProject::ground(vec2(48.0, 112.0).z0()),
            MapProject::ground(vec2(448.0, 112.0).z0()),
            None,
            &LanePatternBuilder::new().build(),
        )
        .unwrap();
        map.environment_mut_for_test()
            .fill_lake(
                &Polygon::centered_rect(vec2(200.0, 152.0), 8.0, 8.0),
                0,
                1.0,
            )
            .unwrap();

        let mut validated = false;
        let result = snap_special_building(
            &map,
            vec2(200.0, 135.0).z0(),
            Size2D::new(40.0, 40.0),
            |_| {
                validated = true;
                Ok(())
            },
        );
        let failure = result.unwrap_err();
        assert_eq!(
            failure.error,
            PlacementError::CommandRejected(Rejection::WetFootprint)
        );
        assert!(failure.attempted.is_some());
        assert!(!validated);
    }
}

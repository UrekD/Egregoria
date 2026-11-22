use geom::{Vec2, Vec3, OBB};
use ordered_float::OrderedFloat;
use prototypes::Size2D;

use super::{Map, ProjectFilter, ProjectKind, Road, RoadID};

const CURB_SETBACK_M: f32 = 0.25;

#[derive(Debug)]
pub struct SpecialBuildArgs {
    pub obb: OBB,
    pub connected_road: RoadID,
    pub height: f32,
}

#[derive(Debug, PartialEq)]
pub enum PlacementError {
    NoRoad,
    NoSidewalk,
    InsufficientFrontage { required_m: f32, available_m: f32 },
    IntersectionOverlap,
    Occupied,
    InvalidTerrain,
}

/// Try nearby roads in distance order, returning the nearest road's error if none fit.
pub fn snap_special_building(
    map: &Map,
    cursor: Vec3,
    size: Size2D,
) -> Result<SpecialBuildArgs, PlacementError> {
    let reach = size.diag() * 0.5;
    if !cursor.is_finite() || !reach.is_finite() || size.w <= 0.0 || size.h <= 0.0 {
        return Err(PlacementError::InvalidTerrain);
    }

    let mut candidates: Vec<_> = map
        .spatial_map()
        .query_around(cursor.xy(), reach, ProjectFilter::ROAD)
        .filter_map(|hit| match hit {
            ProjectKind::Road(id) => map.get(id),
            _ => None,
        })
        .filter_map(|road| {
            let distance2 = road.points().project_dist2(cursor);
            let max_distance = reach + road.width * 0.5;
            (distance2 <= max_distance * max_distance).then_some((
                OrderedFloat(distance2),
                road.id,
                road,
            ))
        })
        .collect();
    candidates.sort_unstable_by_key(|&(distance, id, _)| (distance, id));

    let mut nearest_error = None;
    for (_, _, road) in candidates {
        match snap_to_road(map, cursor.xy(), size, road) {
            Ok(placement) => return Ok(placement),
            Err(error) => {
                nearest_error.get_or_insert(error);
            }
        }
    }
    Err(nearest_error.unwrap_or(PlacementError::NoRoad))
}

fn snap_to_road(
    map: &Map,
    cursor: Vec2,
    size: Size2D,
    road: &Road,
) -> Result<SpecialBuildArgs, PlacementError> {
    // Measure the span in XY, where the footprint lies, so a slope doesn't add frontage.
    let usable = road.interfaced_points().flatten();
    let available_m = usable.length();
    // Measure in a road-aligned frame before rotating the footprint into world space.
    let footprint = OBB::new(Vec2::ZERO, Vec2::X.perpendicular(), size.w, size.h);
    let half_frontage = half_extent(&footprint, Vec2::X);
    let required_m = 2.0 * half_frontage;
    if available_m == 0.0 || required_m > available_m {
        return Err(PlacementError::InsufficientFrontage {
            required_m,
            available_m,
        });
    }

    let (projected, segment_end, tangent) = usable.project_segment_dir(cursor);
    let forward_side = (cursor - projected).dot(tangent.perpendicular()) > 0.0;
    let sidewalks = road.sidewalks(road.src);
    let (sidewalk, side_sign) = if forward_side {
        (sidewalks.outgoing, 1.0)
    } else {
        (sidewalks.incoming, -1.0)
    };
    if sidewalk.is_none() {
        return Err(PlacementError::NoSidewalk);
    }

    let before_segment: f32 = usable
        .segments()
        .take(segment_end - 1)
        .map(|segment| segment.src.distance(segment.dst))
        .sum();
    let along = before_segment + usable[segment_end - 1].distance(projected);
    let along = along.clamp(half_frontage, available_m - half_frontage);
    let (curb_center, tangent) = usable.point_dir_along(along);
    let normal = tangent.perpendicular() * side_sign;
    let setback = half_extent(&footprint, Vec2::Y) + road.width * 0.5 + CURB_SETBACK_M;
    let obb = OBB::new(curb_center + normal * setback, normal, size.w, size.h);

    // The snapped road can also intersect the footprint on a bend.
    let mut occupied = false;
    let blocking = ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING;
    for hit in map.spatial_map().query(obb, blocking) {
        if matches!(hit, ProjectKind::Intersection(_)) {
            return Err(PlacementError::IntersectionOverlap);
        }
        occupied = true;
    }
    if occupied {
        return Err(PlacementError::Occupied);
    }

    let terrain_height = |p| {
        map.environment
            .height(p)
            .filter(|height| height.is_finite())
    };
    let height = terrain_height(obb.center()).ok_or(PlacementError::InvalidTerrain)?;
    if obb.corners.iter().any(|&p| terrain_height(p).is_none()) {
        return Err(PlacementError::InvalidTerrain);
    }
    Ok(SpecialBuildArgs {
        obb,
        connected_road: road.id,
        height,
    })
}

fn half_extent(obb: &OBB, direction: Vec2) -> f32 {
    let [a, b] = obb.axis();
    (a.dot(direction).abs() + b.dot(direction).abs()) * 0.5
}

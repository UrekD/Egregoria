use geom::{vec2, Camera, Vec2, AABB};
use simulation::map::Map;

/// Projects the frustum's intersection with the map's ground and building
/// heights onto XY. Clipping to the map also handles views above the horizon.
pub(crate) fn bounds(camera: &Camera, map: &Map) -> Option<AABB> {
    let top = map
        .environment
        .chunks()
        .map(|(_, chunk)| chunk.max_height())
        .fold(0.0, f32::max)
        + 32.0; // clearance for the elevated HUD badges
    footprint(camera, map.environment.bounds(), top)
}

fn footprint(camera: &Camera, bounds: AABB, top: f32) -> Option<AABB> {
    let mut points = vec![
        bounds.ll,
        vec2(bounds.ur.x, bounds.ll.y),
        bounds.ur,
        vec2(bounds.ll.x, bounds.ur.y),
    ];
    let mut clipped = Vec::with_capacity(9);
    let planes: [fn(geom::Vec4) -> f32; 5] = [
        |p| p.w + p.x,
        |p| p.w - p.x,
        |p| p.w + p.y,
        |p| p.w - p.y,
        |p| p.w - p.z, // near plane in reversed Z
    ];
    let projected_top = camera.proj_cache * geom::vec4(0.0, 0.0, top, 0.0);
    for plane in planes {
        // For each plane keep any XY that could be in the frustum at some
        // height in the slab. The exact per-object test runs after the query.
        let offset = plane(projected_top).max(0.0);
        let distance = |p: Vec2| plane(camera.proj_cache * geom::vec4(p.x, p.y, 0.0, 1.0)) + offset;
        let mut prev = *points.last()?;
        let mut prev_dist = distance(prev);
        for &point in &points {
            let dist = distance(point);
            if (dist >= 0.0) != (prev_dist >= 0.0) {
                clipped.push(prev + (point - prev) * (prev_dist / (prev_dist - dist)));
            }
            if dist >= 0.0 {
                clipped.push(point);
            }
            prev = point;
            prev_dist = dist;
        }
        std::mem::swap(&mut points, &mut clipped);
        clipped.clear();
    }
    let first = *points.first()?;
    let (ll, ur) = points.into_iter().fold((first, first), |(ll, ur), point| {
        (ll.min(point), ur.max(point))
    });
    Some(AABB::new_ll_ur(ll, ur))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec3, Radians};

    #[test]
    fn shallow_view_includes_distant_ground_and_excludes_behind_camera() {
        let mut camera = Camera::new(vec3(3200.0, 915.0, 0.0), 1600.0, 900.0);
        camera.yaw = Radians(0.0);
        camera.pitch = Radians(0.28);
        camera.dist = 650.0;
        camera.update();

        let map = AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(8192.0));
        let view = footprint(&camera, map, 32.0).unwrap();
        assert!(view.contains(vec2(940.0, 915.0)));
        assert!(!view.contains(vec2(5000.0, 915.0)));
        assert!(map.contains(view.ll) && map.contains(view.ur));
    }
}

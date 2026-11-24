use std::cell::RefCell;

use geom::{vec2, Camera, Vec2, AABB, AABB3};
use simulation::map::{Map, MapSubscriber, UpdateType};

#[derive(Default)]
struct TerrainHeights {
    map: usize,
    changes: Option<MapSubscriber>,
    range: [f32; 2],
}

impl TerrainHeights {
    fn get(&mut self, map: &Map) -> [f32; 2] {
        let map_id = std::ptr::from_ref(&map.subscribers) as usize;
        let new_map = self.map != map_id || self.changes.is_none();
        if new_map {
            self.map = map_id;
            self.changes = Some(map.subscribers.subscribe(UpdateType::Terrain));
        }
        let changes = self.changes.as_mut().unwrap();
        let cleared = changes.take_cleared();
        let changed = changes.take_updated_chunks().next().is_some();
        if new_map || cleared || changed {
            // The chunk bounds include the heightmap's below-water range.
            self.range = map
                .environment()
                .chunks()
                .fold([0.0, 0.0], |[low, high], (_, chunk)| {
                    let bounds = chunk.bbox(Vec2::ZERO);
                    [low.min(bounds.ll.z), high.max(bounds.ur.z)]
                });
        }
        self.range
    }
}

thread_local! {
    static TERRAIN_HEIGHTS: RefCell<TerrainHeights> = RefCell::default();
}

pub(super) fn reset_world_cache() {
    TERRAIN_HEIGHTS.with_borrow_mut(|cache| *cache = TerrainHeights::default());
}

#[cfg(test)]
pub(super) fn world_cache_is_clear() -> bool {
    TERRAIN_HEIGHTS.with_borrow(|cache| cache.changes.is_none() && cache.range == [0.0; 2])
}

/// Projects the frustum's intersection with the map's ground and building
/// heights onto XY. Clipping to the map also handles views above the horizon.
pub(crate) fn bounds(camera: &Camera, map: &Map) -> Option<AABB> {
    let [bottom, top] = TERRAIN_HEIGHTS.with_borrow_mut(|cache| cache.get(map));
    footprint(camera, map.environment().bounds(), bottom, top + 32.0) // badge clearance
}

pub(super) fn geometry_bounds(camera: &Camera, bounds: AABB3) -> Option<AABB> {
    footprint(camera, bounds.flatten(), bounds.ll.z, bounds.ur.z)
}

fn footprint(camera: &Camera, bounds: AABB, bottom: f32, top: f32) -> Option<AABB> {
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
    let project_height = |z| camera.proj_cache * geom::vec4(0.0, 0.0, z, 0.0);
    let projected_bottom = project_height(bottom);
    let projected_top = project_height(top);
    for plane in planes {
        // For each plane keep any XY that could be in the frustum at some
        // height in the slab. The exact per-object test runs after the query.
        let offset = plane(projected_top).max(plane(projected_bottom));
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
    fn p2_ui_wire_bounds_include_negative_ground() {
        use simulation::SimulationOptions;

        let test = scenarios::ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let target = vec3(250.0, 250.0, -30.0);
        let mut camera = Camera::new(target, 1600.0, 900.0);
        camera.yaw = Radians(0.0);
        camera.pitch = Radians(0.28);
        camera.dist = 10.0;
        camera.update();

        assert!(bounds(&camera, &test.sim.map())
            .unwrap()
            .contains(target.xy()));
    }

    #[test]
    fn shallow_view_includes_distant_ground_and_excludes_behind_camera() {
        let mut camera = Camera::new(vec3(3200.0, 915.0, 0.0), 1600.0, 900.0);
        camera.yaw = Radians(0.0);
        camera.pitch = Radians(0.28);
        camera.dist = 650.0;
        camera.update();

        let map = AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(8192.0));
        let view = footprint(&camera, map, 0.0, 32.0).unwrap();
        assert!(view.contains(vec2(940.0, 915.0)));
        assert!(!view.contains(vec2(5000.0, 915.0)));
        assert!(map.contains(view.ll) && map.contains(view.ur));
    }
}

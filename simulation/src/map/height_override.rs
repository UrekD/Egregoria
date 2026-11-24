use geom::{pack_height, BaseNode, Shape, Vec2, AABB, NO_OVERRIDE};

use crate::map::terrain::CELL_SIZE;
use crate::map::{
    spend_water_work, Map, ProjectKind, SpatialMapObject, TerrainChunkID, WaterEditError,
    ROAD_Z_OFFSET, TERRAIN_CHUNK_RESOLUTION,
};

pub(crate) type CutHeights = [[u16; TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION];

impl Map {
    pub(crate) fn rebuild_construction_cuts(
        &self,
        chunk: TerrainChunkID,
        is_wet: impl Fn(BaseNode) -> bool,
        work: &mut Option<&mut usize>,
    ) -> Result<CutHeights, WaterEditError> {
        let mut heights = [[NO_OVERRIDE; TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION];
        let chunk_bounds = chunk.bbox();
        for object in self.spatial_map.cut_candidates(chunk_bounds) {
            if let Some(work) = work.as_deref_mut() {
                spend_water_work(work, 1)?;
            }
            let mut add_cut = |bounds: AABB,
                               units: usize,
                               height: &dyn Fn(Vec2) -> Option<f32>|
             -> Result<(), WaterEditError> {
                let lower = bounds.ll.max(chunk_bounds.ll);
                let upper = bounds.ur.min(chunk_bounds.ur);
                if lower.x > upper.x || lower.y > upper.y {
                    return Ok(());
                }
                let bounds = AABB::new_ll_ur(lower, upper).offset(-chunk_bounds.ll);
                let start = (bounds.ll / CELL_SIZE).ceil().max(Vec2::ZERO);
                let end = (bounds.ur / CELL_SIZE)
                    .floor()
                    .min(Vec2::splat((TERRAIN_CHUNK_RESOLUTION - 1) as f32));
                for y in start.y as usize..=end.y as usize {
                    for x in start.x as usize..=end.x as usize {
                        if let Some(work) = work.as_deref_mut() {
                            spend_water_work(work, units)?;
                        }
                        let node = (
                            chunk.0 as usize * TERRAIN_CHUNK_RESOLUTION + x,
                            chunk.1 as usize * TERRAIN_CHUNK_RESOLUTION + y,
                        );
                        if is_wet(node) {
                            continue;
                        }
                        let pos = Vec2::new(x as f32, y as f32) * CELL_SIZE + chunk_bounds.ll;
                        if let Some(z) = height(pos) {
                            let z = pack_height(z).max(NO_OVERRIDE + 1);
                            let old = &mut heights[y][x];
                            *old = if *old == NO_OVERRIDE {
                                z
                            } else {
                                (*old).min(z)
                            };
                        }
                    }
                }
                Ok(())
            };
            match object {
                ProjectKind::Building(id) => {
                    let building = &self.buildings[id];
                    let shape = building.obb.expand(25.0);
                    add_cut(shape.bbox(), 1, &|pos| {
                        shape.contains(pos).then_some(building.height)
                    })?;
                }
                ProjectKind::Intersection(id) => {
                    let intersection = &self.intersections[id];
                    let mut shape = intersection.bcircle();
                    shape.radius *= 2.0;
                    add_cut(shape.bbox(), 1, &|pos| {
                        shape.contains(pos).then_some(intersection.pos.z)
                    })?;
                }
                ProjectKind::Road(id) => {
                    let road = &self.roads[id];
                    let radius = 10.0 + road.width * 3.0;
                    add_cut(road.cut_influence().unwrap(), road.points().len(), &|pos| {
                        let projected = road.points().project_2d(pos);
                        projected
                            .xy()
                            .is_close(pos, radius)
                            .then_some(projected.z - ROAD_Z_OFFSET)
                    })?;
                }
                _ => unreachable!("construction cut index only holds contributors"),
            }
        }
        Ok(heights)
    }
}

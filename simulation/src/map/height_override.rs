use geom::{pack_height, BaseNode, Shape, Vec2, AABB, NO_OVERRIDE, OBB};
use std::collections::BTreeSet;

use crate::map::terrain::CELL_SIZE;
use crate::map::{
    spend_water_work, Intersection, Map, ProjectKind, Road, SpatialMapObject, TerrainChunkID,
    WaterEditError, ROAD_Z_OFFSET, TERRAIN_CHUNK_RESOLUTION,
};

pub(crate) type CutHeights = [[u16; TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION];

#[derive(Clone, Copy)]
pub(super) enum ConstructionCut<'a> {
    Building(OBB, f32),
    Junction(&'a Intersection),
    Road(&'a Road),
}

impl ConstructionCut<'_> {
    fn bounds(self) -> AABB {
        match self {
            Self::Building(shape, _) => shape.expand(25.0).bbox(),
            Self::Junction(junction) => {
                let mut shape = junction.bcircle();
                shape.radius *= 2.0;
                shape.bbox()
            }
            Self::Road(road) => road.cut_influence().unwrap(),
        }
    }
}

impl Map {
    pub(crate) fn rebuild_construction_cuts(
        &self,
        chunk: TerrainChunkID,
        is_wet: impl Fn(BaseNode) -> bool,
        work: &mut Option<&mut usize>,
    ) -> Result<CutHeights, WaterEditError> {
        self.project_construction_cuts(chunk, is_wet, &BTreeSet::new(), &[], work)
    }

    fn construction_cut(&self, object: ProjectKind) -> ConstructionCut<'_> {
        match object {
            ProjectKind::Building(id) => {
                let building = &self.buildings[id];
                ConstructionCut::Building(building.obb, building.height)
            }
            ProjectKind::Intersection(id) => ConstructionCut::Junction(&self.intersections[id]),
            ProjectKind::Road(id) => ConstructionCut::Road(&self.roads[id]),
            _ => unreachable!("construction cut index only holds contributors"),
        }
    }

    pub(super) fn authorize_projected_construction_cuts(
        &self,
        removed: &BTreeSet<ProjectKind>,
        added: &[ConstructionCut<'_>],
        remaining: &mut usize,
    ) -> Result<(), WaterEditError> {
        if !self.land_ownership().is_tiled() {
            return Ok(());
        }
        let mut chunks = BTreeSet::new();
        for cut in removed
            .iter()
            .map(|&object| self.construction_cut(object))
            .chain(added.iter().copied())
        {
            let bounds = cut.bounds();
            for chunk in self
                .environment
                .covered_chunks(AABB::new_ll_ur(bounds.ll, bounds.ur + Vec2::splat(1.0)))
            {
                spend_water_work(remaining, 1)?;
                chunks.insert(chunk);
            }
        }
        let wet = |node| self.environment.is_wet_base_node(node);
        for chunk in chunks {
            spend_water_work(
                remaining,
                TERRAIN_CHUNK_RESOLUTION * TERRAIN_CHUNK_RESOLUTION,
            )?;
            let before = self.rebuild_construction_cuts(chunk, wet, &mut Some(remaining))?;
            let after =
                self.project_construction_cuts(chunk, wet, removed, added, &mut Some(remaining))?;
            for (y, row) in after.iter().enumerate() {
                for (x, &height) in row.iter().enumerate() {
                    let previous = before[y][x];
                    if height == previous {
                        continue;
                    }
                    let node = (
                        chunk.0 as usize * TERRAIN_CHUNK_RESOLUTION + x,
                        chunk.1 as usize * TERRAIN_CHUNK_RESOLUTION + y,
                    );
                    let base = self.environment.heightmap.packed_base_node(node)
                        .expect("covered construction chunk contains its base nodes");
                    // Match the heightmap's effective surface. A cut at or above
                    // the base and an absent override have the same geometry.
                    let effective = |cut| {
                        if cut == NO_OVERRIDE {
                            base
                        } else {
                            base.min(cut)
                        }
                    };
                    if effective(height) != effective(previous) {
                        self.authorize_land_effects(super::PreparedEffects::BaseNode(node))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn project_construction_cuts(
        &self,
        chunk: TerrainChunkID,
        is_wet: impl Fn(BaseNode) -> bool,
        removed: &BTreeSet<ProjectKind>,
        added: &[ConstructionCut<'_>],
        work: &mut Option<&mut usize>,
    ) -> Result<CutHeights, WaterEditError> {
        let mut heights = [[NO_OVERRIDE; TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION];
        let chunk_bounds = chunk.bbox();
        let live = self
            .spatial_map
            .cut_candidates(chunk_bounds)
            .filter(|object| !removed.contains(object))
            .map(|object| self.construction_cut(object));
        for cut in live.chain(added.iter().copied()) {
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
            match cut {
                ConstructionCut::Building(footprint, height) => {
                    let shape = footprint.expand(25.0);
                    add_cut(shape.bbox(), 1, &|pos| {
                        shape.contains(pos).then_some(height)
                    })?;
                }
                ConstructionCut::Junction(intersection) => {
                    let mut shape = intersection.bcircle();
                    shape.radius *= 2.0;
                    add_cut(shape.bbox(), 1, &|pos| {
                        shape.contains(pos).then_some(intersection.pos.z)
                    })?;
                }
                ConstructionCut::Road(road) => {
                    let radius = 10.0 + road.width * 3.0;
                    add_cut(cut.bounds(), road.points().len(), &|pos| {
                        let projected = road.points().project_2d(pos);
                        projected
                            .xy()
                            .is_close(pos, radius)
                            .then_some(projected.z - ROAD_Z_OFFSET)
                    })?;
                }
            }
        }
        Ok(heights)
    }
}

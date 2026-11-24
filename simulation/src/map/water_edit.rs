use super::{
    spend_water_work, CutHeights, Map, ProjectFilter, ProjectKind, TERRAIN_CHUNK_RESOLUTION,
};
use crate::map::terrain::CELL_SIZE;
use crate::map::{
    Lot, LotLayout, PreparedWaterEdit, Road, RoadID, TerrainChunkID, WaterDirtyChunks, WaterEdit,
    WaterEditError, WaterEditOutcome, WaterFootprint, WaterTileCoord, ZoneCellCoord, ZoneGrid,
    MAX_LOT_SIZE, MAX_SLOPE, MAX_WATER_BATCH_WORK, ROAD_Z_OFFSET,
};
use flat_spatial::grid::GridHandle;
use geom::{Intersect, Shape, Vec2, Vec3, AABB, OBB};
use ordered_float::OrderedFloat;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

const MIN_WATER_CLEARANCE_M: f32 = 2.5;
const MAX_DECK_TILE_TESTS: usize = 2_000_000;
const MAX_DECK_FRAGMENTS: usize = 131_072;
const FRONTAGE_SEARCH_M: f32 = MAX_LOT_SIZE * std::f32::consts::SQRT_2 + 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct WaterEditPreview {
    pub changed: bool,
    pub dirty: WaterDirtyChunks,
    pub strict_before: bool,
    pub strict_after: bool,
    pub proposed_surface_dm: Option<i16>,
    pub candidate_extent: Option<AABB>,
}

#[derive(Default)]
struct WaterEditEffects {
    wet_lots: BTreeSet<ProjectKind>,
    covered_zones: Vec<ZoneCellCoord>,
    wet_trees: Vec<(
        TerrainChunkID,
        OrderedFloat<f32>,
        OrderedFloat<f32>,
        GridHandle,
    )>,
    drying_roads: BTreeSet<RoadID>,
    dried: DriedTiles,
    dirty: WaterDirtyChunks,
    cuts: Vec<(TerrainChunkID, CutHeights)>,
}

impl Map {
    pub(crate) fn preview_water_edit(
        &self,
        edit: &WaterEdit,
        zones: &ZoneGrid,
    ) -> Result<WaterEditPreview, WaterEditError> {
        let prepared = self.environment.prepare_water_edit(edit)?;
        let mut remaining = MAX_WATER_BATCH_WORK;
        let effects = self.prepare_water_effects(&prepared, zones, &mut remaining)?;
        Ok(WaterEditPreview {
            changed: prepared.changed(),
            dirty: prepared.dirty().clone().union(effects.dirty),
            strict_before: self.environment.strict_water_siting(),
            strict_after: prepared.strict_water_siting(),
            proposed_surface_dm: prepared.proposed_surface_dm(),
            candidate_extent: prepared
                .dirty()
                .road_support()
                .iter()
                .map(|chunk| chunk.bbox())
                .reduce(AABB::union),
        })
    }

    pub(crate) fn apply_water_edit(
        &mut self,
        edit: &WaterEdit,
        zones: &mut ZoneGrid,
        remaining: &mut usize,
    ) -> Result<WaterEditOutcome, WaterEditError> {
        let prepared = self.environment.prepare_water_edit(edit)?;
        self.commit_water_edit(prepared, zones, remaining)
    }

    pub(crate) fn commit_water_edit(
        &mut self,
        prepared: PreparedWaterEdit,
        zones: &mut ZoneGrid,
        remaining: &mut usize,
    ) -> Result<WaterEditOutcome, WaterEditError> {
        // Prepare against the current topology and commit synchronously. The environment
        // checks both water and base revisions before any staged or cleanup write.
        let effects = self.prepare_water_effects(&prepared, zones, remaining)?;
        let mut outcome = self.environment.commit_prepared_water(prepared)?;
        self.clean_lots_inner(effects.wet_lots.into_iter().collect());
        zones.clear_prepared_cells(effects.covered_zones);
        for (_, _, _, tree) in effects.wet_trees {
            self.environment.trees.remove_maintain(tree);
        }
        for (chunk, cuts) in effects.cuts {
            self.environment.set_overrides(chunk, cuts);
            self.pending_construction_cuts.remove(&chunk);
        }
        for road in effects.drying_roads {
            let filter = |shape, half_width| effects.dried.touches_frontage(shape, half_width);
            Lot::generate_along_road_where(self, road, Some(&filter));
        }
        outcome.dirty = outcome.dirty.union(effects.dirty);
        self.subscribers.dispatch_water(&outcome.dirty);
        Ok(outcome)
    }

    fn prepare_water_effects(
        &self,
        prepared: &PreparedWaterEdit,
        zones: &ZoneGrid,
        remaining: &mut usize,
    ) -> Result<WaterEditEffects, WaterEditError> {
        spend_water_work(remaining, prepared.work())?;
        let mut effects = WaterEditEffects::default();
        let mut terrain = BTreeSet::new();
        let mut road_support = BTreeSet::new();
        let mut candidates = BTreeMap::new();
        for &chunk in prepared.dirty().road_support() {
            spend_water_work(remaining, 1)?;
            let mut objects = BTreeSet::new();
            let bounds = chunk.bbox();
            for object in self
                .spatial_map
                .construction_candidates(bounds.expand(FRONTAGE_SEARCH_M), ProjectFilter::ALL)
            {
                spend_water_work(remaining, 1)?;
                objects.insert(object);
                if matches!(object, ProjectKind::Road(_) | ProjectKind::Intersection(_)) {
                    let bucket = TerrainChunkID::new(object.canonical_position(self));
                    road_support.insert(bucket);
                }
            }
            if !objects.is_empty() {
                candidates.insert(chunk, objects);
            }
        }

        let mut footprints = BTreeMap::new();
        let mut decks = BTreeMap::new();
        let mut deck_tests = MAX_DECK_TILE_TESTS;
        let mut deck_fragments = MAX_DECK_FRAGMENTS;
        let mut frontage_bounds = BTreeMap::new();
        for tile in prepared.affected_tiles() {
            let Some(objects) = candidates.get(&tile.chunk()) else {
                continue;
            };
            let Some(water) = prepared.water_at_tile(&self.environment, tile) else {
                if self.environment.is_water(tile.center()) {
                    let bounds = tile_bounds(tile);
                    for object in objects {
                        spend_water_work(remaining, 1)?;
                        let ProjectKind::Road(id) = *object else {
                            continue;
                        };
                        let frontage = match frontage_bounds.entry(id) {
                            Entry::Occupied(entry) => *entry.get(),
                            Entry::Vacant(entry) => {
                                let road = &self.roads[id];
                                spend_water_work(remaining, road.points().n_points())?;
                                *entry.insert(Lot::allows_frontage(self, road).then(|| {
                                    road.points()
                                        .bbox()
                                        .flatten()
                                        .expand(road.width * 0.5 + FRONTAGE_SEARCH_M)
                                }))
                            }
                        };
                        if frontage.is_some_and(|frontage| bounds.intersects(&frontage)) {
                            effects.drying_roads.insert(id);
                            effects.dried.insert(tile);
                        }
                    }
                }
                continue;
            };

            for &object in objects {
                spend_water_work(remaining, 1)?;
                let mut overlaps = |points: &[Vec2], part| -> Result<bool, WaterEditError> {
                    spend_water_work(remaining, points.len())?;
                    let shape = match footprints.entry((object, part)) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            spend_water_work(remaining, points.len())?;
                            entry.insert(WaterFootprint::new(points))
                        }
                    };
                    Ok(shape
                        .as_ref()
                        .is_some_and(|shape| shape.overlaps_tile(tile)))
                };
                match object {
                    ProjectKind::Building(id) => {
                        let building = &self.buildings[id];
                        if overlaps(&building.obb.corners, 0)?
                            || building
                                .zone
                                .as_ref()
                                .map(|zone| overlaps(zone.poly.as_slice(), 1))
                                .transpose()?
                                .unwrap_or(false)
                        {
                            return Err(WaterEditError::OccupiedFootprint { object, tile });
                        }
                    }
                    ProjectKind::Road(id) => {
                        let road = &self.roads[id];
                        let deck = match decks.entry(id) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => entry.insert(RoadDeck::new(
                                self,
                                road,
                                prepared.dirty().road_support(),
                                remaining,
                                &mut deck_fragments,
                            )?),
                        };
                        for triangle in deck.0.get(&tile.chunk()).into_iter().flatten() {
                            spend_water_work(remaining, 1)?;
                            spend_water_work(&mut deck_tests, 1)?;
                            let Some(z) = triangle.minimum_z_in(tile) else {
                                continue;
                            };
                            if !road.elevated_span() || !triangle.valid_grade {
                                return Err(WaterEditError::TopologyConflict { object, tile });
                            }
                            let minimum_deck =
                                water.sample.surface_z + MIN_WATER_CLEARANCE_M + ROAD_Z_OFFSET;
                            if z < f64::from(minimum_deck) {
                                return Err(WaterEditError::InsufficientClearance { object, tile });
                            }
                        }
                    }
                    ProjectKind::Intersection(id) => {
                        spend_water_work(&mut deck_tests, 1)?;
                        let circle = self.intersections[id].bcircle();
                        let bounds = tile_bounds(tile);
                        let nearest = circle.center.max(bounds.ll).min(bounds.ur);
                        if nearest.distance2(circle.center) < circle.radius * circle.radius {
                            return Err(WaterEditError::TopologyConflict { object, tile });
                        }
                    }
                    ProjectKind::Lot(id) if overlaps(&self.lots[id].shape.corners, 0)? => {
                        effects.wet_lots.insert(object);
                    }
                    _ => {}
                }
            }
        }
        for &id in &effects.drying_roads {
            let road = &self.roads[id];
            // A rejected candidate advances the generator by 2 m. Reserve both sides,
            // the tile search around each pose and the frontage projection before commit.
            let poses = 2 * ((road.length() / 2.0).ceil() as usize + 1);
            let span = ((FRONTAGE_SEARCH_M + road.width + 1.0) / CELL_SIZE).ceil() as usize + 1;
            let pose_work = span
                .saturating_mul(span)
                .saturating_add(road.points().n_points());
            let sizes = if self.lot_layout == LotLayout::Varied {
                3
            } else {
                1
            };
            spend_water_work(
                remaining,
                poses.saturating_mul(pose_work).saturating_mul(sizes),
            )?;
        }

        effects.covered_zones = zones.newly_wet_cells(&self.environment, prepared, remaining)?;
        for &chunk in prepared.dirty().water() {
            let bounds = chunk.bbox();
            for (handle, position) in self.environment.trees.query_aabb(bounds.ll, bounds.ur) {
                spend_water_work(remaining, 1)?;
                if TerrainChunkID::new(position) == chunk
                    && prepared.water_at(&self.environment, position).is_some()
                    && !self.environment.is_water(position)
                {
                    terrain.insert(chunk);
                    effects.wet_trees.push((
                        chunk,
                        OrderedFloat(position.y),
                        OrderedFloat(position.x),
                        handle,
                    ));
                }
            }
        }
        effects.wet_trees.sort_unstable();
        for &chunk in prepared.dirty().road_support() {
            spend_water_work(remaining, 1)?;
            if !self.pending_construction_cuts.contains(&chunk)
                && self
                    .spatial_map
                    .cut_candidates(chunk.bbox())
                    .next()
                    .is_none()
            {
                continue;
            }
            spend_water_work(
                remaining,
                TERRAIN_CHUNK_RESOLUTION * TERRAIN_CHUNK_RESOLUTION,
            )?;
            let cuts = self.rebuild_construction_cuts(
                chunk,
                |node| prepared.is_wet_base_node(&self.environment, node),
                &mut Some(remaining),
            )?;
            effects.cuts.push((chunk, cuts));
            terrain.insert(chunk);
        }
        effects.dirty = WaterDirtyChunks::new(BTreeSet::new(), terrain, road_support);
        Ok(effects)
    }
}

#[derive(Default)]
struct DriedTiles(BTreeMap<TerrainChunkID, [u32; TERRAIN_CHUNK_RESOLUTION]>);

impl DriedTiles {
    fn insert(&mut self, tile: WaterTileCoord) {
        let index = tile.local_index();
        self.0.entry(tile.chunk()).or_default()[index / TERRAIN_CHUNK_RESOLUTION] |=
            1 << (index % TERRAIN_CHUNK_RESOLUTION);
    }

    fn contains(&self, tile: WaterTileCoord) -> bool {
        let index = tile.local_index();
        self.0.get(&tile.chunk()).is_some_and(|rows| {
            rows[index / TERRAIN_CHUNK_RESOLUTION] & (1 << (index % TERRAIN_CHUNK_RESOLUTION)) != 0
        })
    }

    fn touches_frontage(&self, mut lot: OBB, half_width: f32) -> bool {
        let back = (lot.corners[3] - lot.corners[0]).normalize() * (2.0 * half_width + 1.0);
        lot.corners[0] -= back;
        lot.corners[1] -= back;
        let bounds = lot.bbox();
        let Some(shape) = WaterFootprint::new(&lot.corners) else {
            return false;
        };
        let lo = (bounds.ll / CELL_SIZE).floor();
        let hi = (bounds.ur / CELL_SIZE).ceil();
        (lo.x as i16..hi.x as i16).any(|x| {
            (lo.y as i16..hi.y as i16).any(|y| {
                let tile = WaterTileCoord(x, y);
                self.contains(tile) && shape.overlaps_tile(tile)
            })
        })
    }
}

fn tile_bounds(tile: WaterTileCoord) -> AABB {
    AABB::new_ll_size(tile.corner(), Vec2::splat(CELL_SIZE))
}

struct RoadDeck(BTreeMap<TerrainChunkID, Vec<DeckTriangle>>);

impl RoadDeck {
    fn new(
        map: &Map,
        road: &Road,
        affected: &BTreeSet<TerrainChunkID>,
        remaining: &mut usize,
        fragments: &mut usize,
    ) -> Result<Self, WaterEditError> {
        let deck = road.interfaced_points();
        spend_water_work(remaining, deck.n_points())?;
        let mut chunks: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (index, segment) in deck.segments().enumerate() {
            let (from, to) = (segment.src, segment.dst);
            let valid_grade =
                (from.z - to.z).abs() <= MAX_SLOPE * from.xy().distance(to.xy()) + 1e-4;
            let at = |i: usize, pos: Vec3| {
                let before = i.checked_sub(1).map(|i| deck[i]);
                let after = deck.as_slice().get(i + 1).copied();
                let offset = deck_miter(before, pos, after) * (road.width * 0.5);
                [pos + offset.z0(), pos - offset.z0()]
            };
            let [a, b] = at(index, from);
            let [c, d] = at(index + 1, to);
            // Same diagonal as the visible underside mesh.
            for vertices in [[c, b, a], [b, c, d]] {
                let bounds = vertices
                    .iter()
                    .map(|vertex| AABB::new_ll_ur(vertex.xy(), vertex.xy()))
                    .reduce(AABB::union)
                    .unwrap();
                for chunk in map.environment.covered_chunks(bounds) {
                    spend_water_work(remaining, 1)?;
                    if affected.contains(&chunk) {
                        spend_water_work(fragments, 1)?;
                        chunks.entry(chunk).or_default().push(DeckTriangle {
                            vertices,
                            bounds,
                            valid_grade,
                        });
                    }
                }
            }
        }
        Ok(Self(chunks))
    }
}

/// Mirror the deck tessellator's capped XY miter. Endpoint offsets use the same
/// horizontal component of its 3D tangent as the mesh.
fn deck_miter(before: Option<Vec3>, pos: Vec3, after: Option<Vec3>) -> Vec2 {
    if let (Some(before), Some(after)) = (before, after) {
        let incoming = (pos - before).xy().try_normalize();
        let outgoing = (pos - after).xy().try_normalize();
        if let (Some(incoming), Some(outgoing)) = (incoming, outgoing) {
            let cross = incoming.perp_dot(outgoing);
            let normal = -incoming.perpendicular();
            let direction = if cross.abs() < 0.01 {
                normal
            } else {
                (incoming + outgoing)
                    .try_normalize()
                    .map_or(normal, |axis| axis * cross.signum())
            };
            return direction / incoming.perp_dot(direction).max(0.1);
        }
    }
    let tangent = after
        .map(|end| end - pos)
        .or_else(|| before.map(|start| pos - start));
    -tangent
        .and_then(Vec3::try_normalize)
        .unwrap_or_default()
        .xy()
        .perpendicular()
}

#[derive(Clone, Copy)]
struct DeckTriangle {
    vertices: [Vec3; 3],
    bounds: AABB,
    valid_grade: bool,
}

impl DeckTriangle {
    fn minimum_z_in(self, tile: WaterTileCoord) -> Option<f64> {
        let bounds = tile_bounds(tile);
        if !bounds.intersects(&self.bounds)
            || !WaterFootprint::new(&self.vertices.map(Vec3::xy))?.overlaps_tile(tile)
        {
            return None;
        }
        let mut clipped = [[0.0; 3]; 8];
        for (vertex, pos) in clipped.iter_mut().zip(self.vertices) {
            *vertex = [pos.x.into(), pos.y.into(), pos.z.into()];
        }
        let mut len = 3;
        // A triangle clipped to a rectangle has at most seven vertices.
        for (axis, edge, sign) in [
            (0, bounds.ll.x, 1.0),
            (0, bounds.ur.x, -1.0),
            (1, bounds.ll.y, 1.0),
            (1, bounds.ur.y, -1.0),
        ] {
            let mut output = [[0.0; 3]; 8];
            let mut count = 0;
            let mut before = *clipped[..len].last()?;
            for &after in &clipped[..len] {
                let a = (before[axis] - f64::from(edge)) * sign;
                let b = (after[axis] - f64::from(edge)) * sign;
                if (a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0) {
                    let t = a / (a - b);
                    output[count] = std::array::from_fn(|i| before[i] + (after[i] - before[i]) * t);
                    count += 1;
                }
                if b >= 0.0 {
                    output[count] = after;
                    count += 1;
                }
                before = after;
            }
            clipped = output;
            len = count;
        }
        if len < 3 {
            return None;
        }
        let [x, y, _] = clipped[0];
        let twice_area: f64 = clipped[1..len]
            .windows(2)
            .map(|pair| (pair[0][0] - x) * (pair[1][1] - y) - (pair[0][1] - y) * (pair[1][0] - x))
            .sum();
        if twice_area == 0.0 {
            return None;
        }
        clipped[..len]
            .iter()
            .map(|vertex| vertex[2])
            .reduce(f64::min)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Density, LanePatternBuilder, MapProject, WaterBasin, ZoneKind};
    use crate::tests::TestCtx;
    use crate::Simulation;
    use geom::{vec2, Polygon};

    fn lake(center: Vec2, size: f32, level_dm: i16) -> WaterEdit {
        WaterEdit::FillLake {
            basin: WaterBasin::new(&Polygon::centered_rect(center, size, size).0).unwrap(),
            level_dm,
            depth_m: 2.0,
        }
    }

    fn road(sim: &Simulation, y: f32, z: f32) -> RoadID {
        sim.map_mut()
            .make_connection(
                MapProject::ground(vec2(64.0, y).z(z)),
                MapProject::ground(vec2(448.0, y).z(z)),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap()
            .1
    }

    #[test]
    fn w2b_water_edit_cleans_and_restores_local_frontage() {
        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(5, 0).unwrap();
        let road = road(&sim, 128.0, 10.0);
        let paint = OBB::new(vec2(248.0, 164.0), Vec2::X, 16.0, 8.0);
        sim.write::<ZoneGrid>()
            .paint_on_map(
                &sim.map(),
                &paint,
                Some(ZoneKind::Residential),
                Density::Low,
            )
            .unwrap();
        let generation = sim.read::<ZoneGrid>().generation();
        let layout = |sim: &Simulation| {
            let map = sim.map();
            map.lots().values().any(|lot| {
                lot.parent == road
                    && lot
                        .shape
                        .bbox()
                        .intersects(&AABB::centered(vec2(248.0, 168.0), Vec2::splat(12.0)))
            })
        };
        assert!(layout(&sim));
        let edit = lake(vec2(248.0, 168.0), 12.0, 50);
        let preview = sim.preview_water_edit(&edit).unwrap();
        let topology = sim.map().walking_topology_generation;
        let result = sim.edit_water(edit).unwrap();
        assert_eq!(preview.dirty, result.dirty);
        assert!(!layout(&sim));
        assert_eq!(sim.read::<ZoneGrid>().generation(), generation + 1);
        assert!(sim.read::<ZoneGrid>().cells().is_empty());

        sim.edit_water(WaterEdit::RemoveFreshWater {
            body: result.body.unwrap(),
        })
        .unwrap();
        assert!(layout(&sim));
        assert!(sim.read::<ZoneGrid>().cells().is_empty());
        assert_eq!(sim.map().walking_topology_generation, topology);
    }

    #[test]
    fn w2b_water_edit_distinguishes_clear_decks_and_ground_links() {
        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(5, 0).unwrap();
        let ground = road(&sim, 128.0, 10.0);
        let bridge = road(&sim, 224.0, 18.0);
        assert!(sim.map().roads[bridge].elevated_span());

        let ground_edit = lake(vec2(256.0, 128.0), 12.0, 50);
        assert!(matches!(
            sim.preview_water_edit(&ground_edit),
            Err(WaterEditError::TopologyConflict { object: ProjectKind::Road(id), .. }) if id == ground
        ));
        let high = lake(vec2(256.0, 224.0), 12.0, 170);
        let before = sim.hashes();
        assert!(matches!(
            sim.edit_water(high),
            Err(WaterEditError::InsufficientClearance { object: ProjectKind::Road(id), .. }) if id == bridge
        ));
        assert_eq!(sim.hashes(), before);

        let clear = lake(vec2(256.0, 224.0), 12.0, 100);
        let preview = sim.preview_water_edit(&clear).unwrap();
        let result = sim.edit_water(clear).unwrap();
        assert_eq!(preview.dirty, result.dirty);
        assert!(sim.map().roads.contains_key(bridge));
    }
}

use super::{
    spend_water_work, CutHeights, Map, ProjectFilter, ProjectKind, TERRAIN_CHUNK_RESOLUTION,
};
use crate::map::terrain::CELL_SIZE;
use crate::map::{
    deck_triangles, footprint_tiles, prepare_water_providers, DeckTriangle, Intersection,
    IntersectionID, Lot, LotLayout, PreparedWaterEdit, PreparedWaterProviders, Road, RoadID, Roads,
    TerrainChunkID, Turn, UpdateType, WaterDirtyChunks, WaterEdit, WaterEditError,
    WaterEditOutcome, WaterFootprint, WaterProviderImpact, WaterTileCoord, ZoneCellCoord, ZoneGrid,
    FRONTAGE_HEIGHT_TOLERANCE_M, MAX_LOT_SIZE, MAX_SLOPE, MAX_WATER_BATCH_WORK, MIN_LOT_SIZE,
    ROAD_Z_OFFSET, UNIFORM_LOT_SIZE,
};
use flat_spatial::grid::GridHandle;
use flat_spatial::AABBGrid;
use geom::{unpack_height, Intersect, Polygon, Segment, Shape, Vec2, Vec3, AABB, NO_OVERRIDE, OBB};
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
    pub provider_impact: WaterProviderImpact,
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
    new_lots: Vec<StagedLot>,
    dirty: WaterDirtyChunks,
    cuts: BTreeMap<TerrainChunkID, CutHeights>,
    water_providers: PreparedWaterProviders,
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
            provider_impact: effects.water_providers.impact,
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
        for lot in effects.new_lots {
            let id = Lot::insert(self, lot.road, lot.shape, lot.height);
            self.subscribers.dispatch(UpdateType::Road, &self.lots[id]);
        }
        outcome.dirty = outcome.dirty.union(effects.dirty);
        if outcome.changed {
            self.water_services.water_edited(
                &self.environment,
                &outcome.dirty,
                effects.water_providers,
            );
        }
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
                if let ProjectKind::Road(id) = object {
                    let road = &self.roads[id];
                    objects.extend([
                        ProjectKind::Intersection(road.src),
                        ProjectKind::Intersection(road.dst),
                    ]);
                }
                if matches!(object, ProjectKind::Road(_) | ProjectKind::Intersection(_)) {
                    road_support.insert(TerrainChunkID::new(object.canonical_position(self)));
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
        let mut junctions = BTreeMap::new();
        let mut drying_roads = BTreeSet::new();
        let mut dried = DriedTiles::default();
        for tile in prepared.affected_tiles() {
            let Some(objects) = candidates.get(&tile.chunk()) else {
                continue;
            };
            let Some(water) = prepared.water_at_tile(&self.environment, tile) else {
                if self.environment.is_water(tile.center()) {
                    dried.insert(tile);
                    for object in objects {
                        spend_water_work(remaining, 1)?;
                        if let ProjectKind::Road(id) = *object {
                            if Lot::allows_frontage(self, &self.roads[id]) {
                                drying_roads.insert(id);
                            }
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
                        for &(triangle, valid_grade) in
                            deck.0.get(&tile.chunk()).into_iter().flatten()
                        {
                            spend_water_work(remaining, 1)?;
                            spend_water_work(&mut deck_tests, 1)?;
                            let Some(z) = triangle.minimum_z_in(tile) else {
                                continue;
                            };
                            if !road.elevated_span() || !valid_grade {
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
                        let junction = match junctions.entry(id) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                entry.insert(JunctionFootprint::new(self, id, remaining)?)
                            }
                        };
                        if let Some(junction) = junction {
                            if junction.bounds.intersects(&tile_bounds(tile)) {
                                spend_water_work(remaining, junction.vertices)?;
                                if junction.shape.overlaps_tile(tile) {
                                    if !junction.elevated {
                                        return Err(WaterEditError::TopologyConflict {
                                            object,
                                            tile,
                                        });
                                    }
                                    if junction.height
                                        < water.sample.surface_z + MIN_WATER_CLEARANCE_M
                                    {
                                        return Err(WaterEditError::InsufficientClearance {
                                            object,
                                            tile,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    ProjectKind::Lot(id) if overlaps(&self.lots[id].shape.corners, 0)? => {
                        effects.wet_lots.insert(object);
                    }
                    _ => {}
                }
            }
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
            effects.cuts.insert(chunk, cuts);
            terrain.insert(chunk);
        }
        if !drying_roads.is_empty() {
            effects.new_lots = DryingFrontage {
                map: self,
                prepared,
                cuts: &effects.cuts,
                removed: &effects.wet_lots,
                dried: &dried,
            }
            .stage(drying_roads, remaining)?;
        }
        effects.dirty = WaterDirtyChunks::new(BTreeSet::new(), terrain, road_support);
        effects.water_providers = prepare_water_providers(self, prepared, remaining)?;
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

    fn in_chunk(&self, chunk: TerrainChunkID) -> impl Iterator<Item = WaterTileCoord> + '_ {
        let resolution = TERRAIN_CHUNK_RESOLUTION as i16;
        self.0
            .get(&chunk)
            .into_iter()
            .flatten()
            .enumerate()
            .flat_map(move |(y, &row)| {
                let mut row = row;
                std::iter::from_fn(move || {
                    if row == 0 {
                        return None;
                    }
                    let x = row.trailing_zeros() as i16;
                    row &= row - 1;
                    Some(WaterTileCoord(
                        chunk.0 * resolution + x,
                        chunk.1 * resolution + y as i16,
                    ))
                })
            })
    }
}

fn tile_bounds(tile: WaterTileCoord) -> AABB {
    AABB::new_ll_size(tile.corner(), Vec2::splat(CELL_SIZE))
}

struct RoadDeck(BTreeMap<TerrainChunkID, Vec<(DeckTriangle, bool)>>);

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
            for triangle in deck_triangles(road, index, 0.0, 1.0) {
                for chunk in map.environment.covered_chunks(triangle.bounds) {
                    spend_water_work(remaining, 1)?;
                    if affected.contains(&chunk) {
                        spend_water_work(fragments, 1)?;
                        chunks
                            .entry(chunk)
                            .or_default()
                            .push((triangle, valid_grade));
                    }
                }
            }
        }
        Ok(Self(chunks))
    }
}

struct JunctionFootprint {
    shape: WaterFootprint,
    bounds: AABB,
    vertices: usize,
    elevated: bool,
    height: f32,
}

impl JunctionFootprint {
    fn new(
        map: &Map,
        id: IntersectionID,
        remaining: &mut usize,
    ) -> Result<Option<Self>, WaterEditError> {
        let inter = &map.intersections[id];
        let outline = Map::water_junction_outline(inter, &map.roads, remaining)?;
        let Some(shape) = WaterFootprint::new(&outline.0) else {
            return Ok(None);
        };
        let ground = map
            .environment
            .heightmap
            .base_height(inter.pos.xy())
            .unwrap_or(inter.pos.z)
            .max(0.0);
        let elevated = (inter.pos.z >= ground + 0.5 || map.environment.is_water(inter.pos.xy()))
            && inter
                .roads
                .iter()
                .any(|&road| map.roads[road].elevated_span());
        Ok(Some(Self {
            shape,
            bounds: outline.bbox(),
            vertices: outline.len(),
            elevated,
            height: inter.pos.z,
        }))
    }
}

impl Map {
    /// Final full-width junction, from the same clipped arms and turn splines as
    /// the renderer. A single arm connects to itself to form the dead-end cap.
    pub(crate) fn water_junction_outline(
        junction: &Intersection,
        roads: &Roads,
        remaining: &mut usize,
    ) -> Result<Polygon, WaterEditError> {
        junction_outline(junction, roads, remaining)
    }
}

fn junction_outline(
    junction: &Intersection,
    roads: &Roads,
    remaining: &mut usize,
) -> Result<Polygon, WaterEditError> {
    let mut arms = Vec::new();
    for &id in &junction.roads {
        spend_water_work(remaining, 1)?;
        let road = &roads[id];
        let points = road.interfaced_points();
        let pose = if road.src == junction.id {
            points.first_dir().map(|dir| (points.first(), dir.xy()))
        } else {
            points.last_dir().map(|dir| (points.last(), -dir.xy()))
        };
        if let Some((point, dir)) = pose {
            let normal = dir.perpendicular() * (road.width * 0.5);
            arms.push((point.xy() - normal, point.xy() + normal, dir));
        }
    }
    let mut outline = Polygon::default();
    let center = junction.pos.xy();
    for (&(left, _, src), &(_, right, dst)) in arms.iter().zip(arms.iter().cycle().skip(1)) {
        if junction.is_roundabout() {
            if let Some(policy) = junction.turn_policy.roundabout {
                let a = (left - center).try_normalize();
                let b = (right - center).try_normalize();
                if a.zip(b)
                    .is_some_and(|(a, b)| a.angle(b).abs() >= 21.0_f32.to_radians())
                {
                    spend_water_work(remaining, (policy.radius.abs() / 10.0).ceil() as usize + 1)?;
                    for point in Turn::gen_roundabout(
                        left.z0(),
                        right.z0(),
                        -src,
                        dst,
                        policy.radius + 3.0,
                        center,
                    ) {
                        spend_water_work(remaining, 1)?;
                        outline.push(point);
                    }
                    continue;
                }
            }
        }
        let spline = Turn::spline(left, right, -src, dst);
        for point in spline.smart_points(1.0, 0.0, 1.0) {
            spend_water_work(remaining, 1)?;
            outline.push(point);
        }
    }
    spend_water_work(remaining, outline.len())?;
    // Match Polygon::simplify using a linear pass over the original vertices.
    Ok(Polygon(
        outline
            .iter()
            .enumerate()
            .filter_map(|(i, &point)| {
                let prev = *outline.get_prev(i);
                let next = *outline.get_next(i);
                (!prev.approx_eq(point)
                    && !(point - prev)
                        .normalize()
                        .approx_eq((next - point).normalize()))
                .then_some(point)
            })
            .collect(),
    ))
}

struct FrontageDeck(AABBGrid<usize, AABB>);

impl FrontageDeck {
    const CELL: i32 = 50;

    fn new(road: &Road, remaining: &mut usize) -> Result<Self, WaterEditError> {
        let mut grid = AABBGrid::new(Self::CELL);
        for (index, segment) in road.interfaced_points().segments().enumerate() {
            let bounds = segment.flatten().bbox();
            let cell = Self::CELL as f32;
            let span = (bounds.ur / cell).floor() - (bounds.ll / cell).floor();
            spend_water_work(
                remaining,
                (span.x as usize + 1).saturating_mul(span.y as usize + 1),
            )?;
            grid.insert(bounds, index);
        }
        Ok(Self(grid))
    }

    fn project(
        &self,
        road: &Road,
        point: Vec2,
        remaining: &mut usize,
    ) -> Result<Option<(Vec3, usize)>, WaterEditError> {
        let bounds = AABB::new_ll_ur(point, point).expand(road.width * 0.5 + 2.0);
        let points = road.interfaced_points();
        let mut nearest = None;
        for handle in self.0.query_broad(bounds) {
            spend_water_work(remaining, 1)?;
            let object = self.0.get(handle).unwrap();
            let index = object.obj;
            let (a, b) = (points[index], points[index + 1]);
            let t = Segment {
                src: a.xy(),
                dst: b.xy(),
            }
            .project_t(point);
            let projected = a + (b - a) * t;
            let key = (OrderedFloat(point.distance(projected.xy())), index);
            if nearest.is_none_or(|(best, _)| key < best) {
                nearest = Some((key, projected));
            }
        }
        Ok(nearest.map(|((_, index), projected)| (projected, index + 1)))
    }
}

struct StagedLot {
    road: RoadID,
    shape: OBB,
    height: f32,
}

struct DryingFrontage<'a> {
    map: &'a Map,
    prepared: &'a PreparedWaterEdit,
    cuts: &'a BTreeMap<TerrainChunkID, CutHeights>,
    removed: &'a BTreeSet<ProjectKind>,
    dried: &'a DriedTiles,
}

impl DryingFrontage<'_> {
    fn stage(
        &self,
        roads: BTreeSet<RoadID>,
        remaining: &mut usize,
    ) -> Result<Vec<StagedLot>, WaterEditError> {
        let mut lots: Vec<StagedLot> = Vec::new();
        let mut staged: AABBGrid<usize, AABB> = AABBGrid::new(50);
        for id in roads {
            let road = &self.map.roads[id];
            let windows = self.windows(road, remaining)?;
            if windows.is_empty() {
                continue;
            }
            let deck = FrontageDeck::new(road, remaining)?;
            let sidewalks = road.sidewalks(road.src);
            for (side, sidewalk) in [(1.0, sidewalks.outgoing), (-1.0, sidewalks.incoming)] {
                if sidewalk.is_none() {
                    continue;
                }
                // This iterator skips directly to each local window. Traversing the
                // intervening profile is charged once per side, independently of poses.
                spend_water_work(remaining, road.points().n_points())?;
                let mut along = road.points().points_dirs_manual();
                for &(start, end) in &windows {
                    let mut distance = (start / 2.0).ceil() * 2.0;
                    while distance <= end {
                        spend_water_work(remaining, 1)?;
                        let Some((pos, dir)) = along.next(distance) else {
                            break;
                        };
                        let axis = side * dir.perp_up();
                        let at = pos + axis * (road.width * 0.5 + 1.0);
                        let Some(height) = self.height(at.xy()) else {
                            distance += 2.0;
                            continue;
                        };
                        if (height - at.z).abs() > FRONTAGE_HEIGHT_TOLERANCE_M {
                            distance += 2.0;
                            continue;
                        }
                        let sizes = [MIN_LOT_SIZE, UNIFORM_LOT_SIZE, MAX_LOT_SIZE];
                        let preferred = match self.map.lot_layout {
                            LotLayout::Uniform => 1,
                            LotLayout::Varied => {
                                let first = road.points().first();
                                let last = road.points().last();
                                (common::rand::rand4(
                                    first.x + last.x,
                                    first.y + last.y,
                                    side * road.length(),
                                    distance,
                                ) * 3.0) as usize
                            }
                        };
                        let alternatives = sizes.iter().rev().copied().filter(|&size| {
                            self.map.lot_layout == LotLayout::Varied && size != sizes[preferred]
                        });
                        let mut step = 2.0;
                        for size in std::iter::once(sizes[preferred]).chain(alternatives) {
                            spend_water_work(remaining, 1)?;
                            let shape =
                                OBB::new(at.xy() + axis.xy() * size * 0.5, axis.xy(), size, size);
                            let mut touches =
                                self.touches_dried_frontage(shape, road.width, remaining)?;
                            let usable = Lot::check_dry_frontage(
                                road,
                                &shape,
                                |point, remaining| deck.project(road, point, remaining),
                                |point| self.height(point),
                                |tile| {
                                    touches |= self.dried.contains(tile);
                                    self.prepared
                                        .water_at_tile(&self.map.environment, tile)
                                        .is_some()
                                },
                                remaining,
                            )?;
                            if !usable || !touches || self.obstructed(shape, remaining)? {
                                continue;
                            }
                            let mut conflicts = false;
                            for (_, _, &index) in staged.query(shape.bbox()) {
                                spend_water_work(remaining, 1)?;
                                conflicts |= shape.intersects(&lots[index].shape);
                            }
                            if !conflicts {
                                staged.insert(shape.bbox(), lots.len());
                                lots.push(StagedLot {
                                    road: id,
                                    shape,
                                    height,
                                });
                                step += size * 0.5 + MIN_LOT_SIZE * 0.5;
                                break;
                            }
                        }
                        distance += step;
                    }
                }
            }
        }
        Ok(lots)
    }

    fn windows(
        &self,
        road: &Road,
        remaining: &mut usize,
    ) -> Result<Vec<(f32, f32)>, WaterEditError> {
        let mut start = 0.0;
        let mut windows = Vec::new();
        let radius = FRONTAGE_SEARCH_M.max(5.0 * road.width + MAX_LOT_SIZE * 0.5);
        for segment in road.points().segments() {
            spend_water_work(remaining, 1)?;
            let length = segment.src.distance(segment.dst);
            let a = segment.src.xy();
            let b = segment.dst.xy();
            let bounds = AABB::new_ll_ur(a.min(b), a.max(b)).expand(radius);
            for chunk in self.map.environment.covered_chunks(bounds) {
                spend_water_work(remaining, 1)?;
                if self.dried.0.contains_key(&chunk) {
                    spend_water_work(remaining, TERRAIN_CHUNK_RESOLUTION)?;
                }
                for tile in self.dried.in_chunk(chunk) {
                    spend_water_work(remaining, 1)?;
                    if let Some((lo, hi)) = segment_window(a, b, tile_bounds(tile).expand(radius)) {
                        windows.push((start + lo * length, start + hi * length));
                    }
                }
            }
            start += length;
        }
        spend_water_work(
            remaining,
            windows
                .len()
                .saturating_mul(windows.len().max(1).ilog2() as usize + 1),
        )?;
        windows.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for (lo, hi) in windows {
            if let Some(last) = merged.last_mut().filter(|last| lo <= last.1) {
                last.1 = last.1.max(hi);
            } else {
                merged.push((lo, hi));
            }
        }
        Ok(merged)
    }

    fn touches_dried_frontage(
        &self,
        mut shape: OBB,
        width: f32,
        remaining: &mut usize,
    ) -> Result<bool, WaterEditError> {
        let back = (shape.corners[3] - shape.corners[0]).normalize() * (width + 1.0);
        shape.corners[0] -= back;
        shape.corners[1] -= back;
        let footprint = WaterFootprint::new(&shape.corners).unwrap();
        for tile in footprint_tiles(shape.bbox()) {
            spend_water_work(remaining, 1)?;
            if self.dried.contains(tile) && footprint.overlaps_tile(tile) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn obstructed(&self, shape: OBB, remaining: &mut usize) -> Result<bool, WaterEditError> {
        let mut candidates = BTreeSet::new();
        for object in self
            .map
            .spatial_map
            .construction_candidates(shape.bbox(), ProjectFilter::ALL)
        {
            spend_water_work(remaining, 1)?;
            if !self.removed.contains(&object) {
                candidates.insert(object);
            }
        }
        let polygon = Polygon::from(shape.corners.as_slice());
        for object in candidates {
            let blocked = match object {
                ProjectKind::Road(id) => {
                    let road = &self.map.roads[id];
                    // Includes flattening and all four edges of the lot polygon.
                    spend_water_work(remaining, road.points().n_points().saturating_mul(6))?;
                    polygon.intersects(&road.boldline())
                }
                ProjectKind::Intersection(id) => {
                    shape.intersects(&self.map.intersections[id].bcircle())
                }
                ProjectKind::Building(id) => {
                    let building = &self.map.buildings[id];
                    if let Some(zone) = &building.zone {
                        spend_water_work(remaining, zone.poly.len().saturating_mul(5))?;
                        shape.intersects(&zone.poly) || shape.intersects(&building.obb)
                    } else {
                        shape.intersects(&building.obb)
                    }
                }
                ProjectKind::Lot(id) => shape.intersects(&self.map.lots[id].shape),
                ProjectKind::Ground => false,
            };
            if blocked {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // Sample the prospective packed terrain with the construction-cut overrides
    // just prepared for this edit. Match Environment::height at the outer edge.
    fn height(&self, point: Vec2) -> Option<f32> {
        let env = &self.map.environment;
        let node_height = |point: Vec2| {
            let chunk = TerrainChunkID::new(point);
            if !self.prepared.dirty().terrain().contains(&chunk) && !self.cuts.contains_key(&chunk)
            {
                return env.heightmap.height_nearest(point);
            }
            let node = (point / CELL_SIZE).floor() * CELL_SIZE;
            let base = self.prepared.base_height(env, node)?;
            let tile = WaterTileCoord((node.x / CELL_SIZE) as i16, (node.y / CELL_SIZE) as i16);
            let index = tile.local_index();
            let cut = self.cuts.get(&chunk).map(|cuts| {
                cuts[index / TERRAIN_CHUNK_RESOLUTION][index % TERRAIN_CHUNK_RESOLUTION]
            });
            Some(
                cut.filter(|&cut| cut != NO_OVERRIDE)
                    .map_or(base, |cut| base.min(unpack_height(cut))),
            )
        };
        let ll = node_height(point)?;
        let height = match (
            node_height(point + Vec2::x(CELL_SIZE)),
            node_height(point + Vec2::y(CELL_SIZE)),
            node_height(point + Vec2::splat(CELL_SIZE)),
        ) {
            (Some(lr), Some(ul), Some(ur)) => {
                let p = (point / CELL_SIZE).fract();
                let lower = ll + p.x * (lr - ll);
                let upper = ul + p.x * (ur - ul);
                lower + p.y * (upper - lower)
            }
            _ => ll,
        };
        Some(height.max(0.0))
    }
}

fn segment_window(a: Vec2, b: Vec2, bounds: AABB) -> Option<(f32, f32)> {
    let mut start = 0.0_f32;
    let mut end = 1.0_f32;
    for (src, dst, min, max) in [
        (a.x, b.x, bounds.ll.x, bounds.ur.x),
        (a.y, b.y, bounds.ll.y, bounds.ur.y),
    ] {
        let delta = dst - src;
        if delta == 0.0 {
            if src < min || src > max {
                return None;
            }
        } else {
            let enter = (min - src) / delta;
            let exit = (max - src) / delta;
            start = start.max(enter.min(exit));
            end = end.min(enter.max(exit));
        }
    }
    (start <= end).then_some((start, end))
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
    fn w2b_water_new_street_end_lots_are_buildable() {
        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
        sim.map_mut().set_lot_layout(LotLayout::Uniform);
        sim.edit_water(lake(vec2(248.0, 392.0), 8.0, 50)).unwrap();

        let mut map = sim.map_mut();
        assert!(map.environment().strict_water_siting());
        let (_, road) = map
            .make_connection(
                MapProject::ground(vec2(100.0, 128.0).z(10.0)),
                MapProject::ground(vec2(440.0, 128.0).z(10.0)),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let end_lots = [false, true].map(|north| {
            map.lots()
                .values()
                .filter(|lot| lot.parent == road && (lot.shape.center().y > 128.0) == north)
                .min_by(|a, b| a.shape.center().x.total_cmp(&b.shape.center().x))
                .expect("a lot near the start of the street on each side")
                .id
        });
        let houses = end_lots.map(|lot| map.build_house(lot));
        assert!(
            houses.iter().all(Option::is_some),
            "both end lots should be buildable, got {houses:?}"
        );
    }

    #[test]
    fn w2b_water_shallow_merge_preserves_dry_end_lot() {
        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
        sim.edit_water(lake(vec2(248.0, 392.0), 8.0, 50)).unwrap();
        let mut map = sim.map_mut();
        map.set_lot_layout(LotLayout::Uniform);

        let ground = |x| MapProject::ground(vec2(x, 128.0).z(10.0));
        let junction = |x, id| MapProject {
            pos: ground(x).pos,
            kind: ProjectKind::Intersection(id),
        };
        let street = LanePatternBuilder::new().build();
        let approach = LanePatternBuilder::new().speed_limit(8.0).build();
        let (start, _) = map
            .make_connection(ground(40.0), ground(100.0), None, &approach)
            .unwrap();
        let (middle, first) = map
            .make_connection(junction(100.0, start), ground(270.0), None, &street)
            .unwrap();
        let lot = map
            .lots()
            .values()
            .filter(|lot| lot.parent == first)
            .min_by(|a, b| a.shape.center().x.total_cmp(&b.shape.center().x))
            .unwrap()
            .id;
        map.make_connection(junction(270.0, middle), ground(440.0), None, &street)
            .unwrap();
        assert!(!map.roads().contains_key(first));
        assert!(
            map.build_house(lot).is_some(),
            "end lot was lost in the merge"
        );
    }

    #[test]
    fn w2b_water_edit_cleans_and_restores_local_frontage() {
        use crate::map::{
            water_provider_site, BuildingKind, WaterProviderImpactSample, WaterServiceRole,
            WaterSite, WaterSiteError,
        };
        use prototypes::{try_prototype, GoodsCompanyID};

        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(5, 0).unwrap();
        let road = road(&sim, 128.0, 10.0);
        let pump_kind = GoodsCompanyID::new("water-pump");
        let pump = sim
            .map_mut()
            .build_special_building(
                &OBB::new(vec2(248.0, 208.0), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(pump_kind),
                try_prototype(pump_kind).unwrap().bgen,
                None,
                Some(road),
            )
            .unwrap();
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

        let body = result.body.unwrap();
        let drain = WaterEdit::RemoveFreshWater { body };
        assert!(matches!(
            water_provider_site(&sim.map(), pump),
            Some(Ok(WaterSite::Shore { contact, .. })) if contact.body == body
        ));
        let preview = sim.preview_water_edit(&drain).unwrap();
        let failure = Err(WaterSiteError::NoEligibleShore(WaterServiceRole::Intake));
        assert_eq!(preview.provider_impact.count, 1);
        assert_eq!(
            preview.provider_impact.sample,
            [WaterProviderImpactSample {
                building: pump,
                proposed_site: failure,
            }]
        );

        let result = sim.edit_water(drain).unwrap();
        assert_eq!(preview.dirty, result.dirty);
        assert_eq!(water_provider_site(&sim.map(), pump), Some(failure));
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

    #[test]
    fn w2b_water_edit_rejects_dead_end_cap_without_deck_overlap() {
        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
        let (_, road) = sim
            .map_mut()
            .make_connection(
                MapProject::ground(vec2(116.0, 136.0).z(10.0)),
                MapProject::ground(vec2(400.0, 136.0).z(10.0)),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let junction = sim.map().roads[road].src;
        assert!(sim.map().roads[road].interfaced_points().first().x > 128.0);
        let edit = lake(vec2(120.0, 136.0), 8.0, 50);
        let expected = Err(WaterEditError::TopologyConflict {
            object: ProjectKind::Intersection(junction),
            tile: WaterTileCoord(7, 8),
        });
        let before = sim.hashes();
        assert_eq!(sim.preview_water_edit(&edit).map(|_| ()), expected);
        assert_eq!(sim.edit_water(edit).map(|_| ()), expected);
        assert_eq!(sim.hashes(), before);
    }

    #[test]
    fn w2b_water_edit_restores_one_tile_beside_a_long_road() {
        drop(TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(32, 0).unwrap();
        let (_, road) = sim
            .map_mut()
            .make_connection(
                MapProject::ground(vec2(64.0, 128.0).z(10.0)),
                MapProject::ground(vec2(16064.0, 128.0).z(10.0)),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let (center, flooded) = sim
            .map()
            .lots()
            .values()
            .filter(|lot| lot.parent == road && lot.shape.bbox().ur.y > 160.0)
            .find_map(|lot| {
                let x = lot.shape.center().x;
                (180.0..500.0).contains(&x).then_some((
                    vec2((x / CELL_SIZE).floor() * CELL_SIZE + 8.0, 168.0),
                    lot.id,
                ))
            })
            .expect("a lot reaching the one-tile lake");
        let body = sim.edit_water(lake(center, 8.0, 50)).unwrap().body.unwrap();
        assert!(!sim.map().lots().contains_key(flooded));
        let local = |lot: &Lot| lot.shape.center().distance2(center) < 80.0_f32.powi(2);
        let before: BTreeSet<_> = sim.map().lots().values().map(|lot| lot.id).collect();
        let drain = WaterEdit::RemoveFreshWater { body };
        let preview = sim.preview_water_edit(&drain).unwrap();
        assert_eq!(sim.edit_water(drain).unwrap().dirty, preview.dirty);
        let map = sim.map();
        let restored: Vec<_> = map
            .lots()
            .values()
            .filter(|lot| !before.contains(&lot.id))
            .collect();
        assert!(!restored.is_empty());
        assert!(restored.iter().all(|lot| lot.parent == road && local(lot)));
    }

    #[test]
    fn w2b_water_edit_charges_dense_frontage_collisions_during_prepare() {
        drop(TestCtx::new_fast());
        let prepare = |obstacle: Vec2| {
            let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
            let road = road(&sim, 128.0, 10.0);
            let body = sim
                .edit_water(lake(vec2(248.0, 168.0), 8.0, 50))
                .unwrap()
                .body
                .unwrap();
            let mut map = sim.map_mut();
            // The same number of existing objects are discovered in the changed
            // chunk, but only the near ones reach the per-lot collision query.
            for _ in 0..48 {
                let id = map.lots.insert_with_key(|id| Lot {
                    id,
                    parent: road,
                    kind: crate::map::LotKind::Unassigned,
                    shape: OBB::new(obstacle, Vec2::Y, 3.0, 3.0),
                    height: 10.0,
                });
                let map = &mut *map;
                map.spatial_map.insert(&map.lots[id]);
            }
            let prepared = map
                .environment
                .prepare_water_edit(&WaterEdit::RemoveFreshWater { body })
                .unwrap();
            let mut zones = sim.write::<ZoneGrid>();
            let mut remaining = MAX_WATER_BATCH_WORK;
            map.prepare_water_effects(&prepared, &zones, &mut remaining)
                .unwrap();
            let used = MAX_WATER_BATCH_WORK - remaining;
            let before = (map.lots.len(), map.environment.water_revision());
            assert!(matches!(
                map.commit_water_edit(prepared, &mut zones, &mut (used - 1)),
                Err(WaterEditError::Limit)
            ));
            assert_eq!(before, (map.lots.len(), map.environment.water_revision()));
            assert!(map.environment.is_water(vec2(248.0, 168.0)));
            used
        };
        assert!(prepare(vec2(237.0, 155.0)) > prepare(vec2(440.0, 300.0)));
    }

    #[test]
    fn w2b_water_deck_boundary_contacts_and_clipped_height() {
        let triangle = |a, b, c| DeckTriangle::new([a, b, c]);
        let tile = WaterTileCoord(0, 0);
        let edge = triangle(
            vec2(16.0, 0.0).z(3.0),
            vec2(16.0, 16.0).z(5.0),
            vec2(24.0, 8.0).z(6.0),
        );
        assert_eq!(edge.minimum_z_in(tile), None);
        let diagonal = triangle(
            vec2(8.0, 24.0).z(3.0),
            vec2(24.0, 8.0).z(5.0),
            vec2(24.0, 24.0).z(6.0),
        );
        assert_eq!(diagonal.minimum_z_in(tile), None);
        let crossing = triangle(
            vec2(-16.0, 8.0).z(0.0),
            vec2(32.0, 8.0).z(12.0),
            vec2(32.0, 12.0).z(12.0),
        );
        assert_eq!(crossing.minimum_z_in(tile), Some(4.0));
        let degenerate = triangle(
            vec2(1.0, 1.0).z(3.0),
            vec2(7.0, 7.0).z(5.0),
            vec2(9.0, 9.0).z(6.0),
        );
        assert_eq!(degenerate.minimum_z_in(tile), None);
    }
}

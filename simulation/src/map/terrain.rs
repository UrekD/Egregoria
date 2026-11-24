use std::ops::Mul;

use flat_spatial::storage::CellIdx;
use flat_spatial::Grid;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};

use common::FastSet;
use egui_inspect::egui::ahash::HashSetExt;
use geom::{
    lerp, pack_height, unpack_height, vec2, BaseNodePatch, Intersect, Radians, Ray3, Vec2, Vec3,
    AABB,
};
use prototypes::{Tick, DELTA};

use crate::map::procgen::heightmap;
use crate::map::procgen::heightmap::tree_density;
use crate::map::water::{
    validate_map_dimensions, PreparedWaterEdit, WaterEditError, WaterState, MAX_WATER_BASE_WRITES,
    MAX_WATER_BATCH_WORK, MAX_WATER_CHUNKS_PER_AXIS,
};
use crate::utils::rand_provider::RandProvider;
use crate::{MapPreset, NewGameOptions, SimulationOptions};

pub type TerrainChunkID = common::ChunkID_512;

pub const TERRAIN_CHUNK_RESOLUTION: usize = 32;

pub(super) const CELL_SIZE: f32 = TerrainChunkID::SIZE_F32 / TERRAIN_CHUNK_RESOLUTION as f32; // 512 / 32 = 16

const TREE_GRID_SIZE: usize = 256;

pub type Chunk = geom::HeightmapChunk<TERRAIN_CHUNK_RESOLUTION, { TerrainChunkID::SIZE }>;
pub type Heightmap = geom::Heightmap<TERRAIN_CHUNK_RESOLUTION, { TerrainChunkID::SIZE }>;

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub pos: Vec2,
    pub size: f32,
    pub col: f32,
    pub dir: Vec2,
}

#[derive(Clone)]
pub struct Environment {
    pub(super) heightmap: Heightmap,
    pub(super) water: WaterState,
    pub(super) base_revision: u64,
    pub trees: Grid<Tree, Vec2>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TerraformKind {
    Elevation,
    Smooth,
    Level,
    Slope,
    Erode,
}

impl Default for Environment {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl Environment {
    pub fn new(w: u16, h: u16) -> Self {
        Self::generate(
            w,
            h,
            MapPreset::Procedural,
            0,
            heightmap::LEGACY_VALLEY_AXIS,
        )
    }

    pub(crate) fn from_options(options: SimulationOptions) -> Self {
        let size = options.terrain_size;
        let mut valley_axis = heightmap::LEGACY_VALLEY_AXIS;
        // Low-level small maps used by scenarios retain their legacy terrain.
        if size >= NewGameOptions::MIN_TERRAIN_CHUNKS {
            valley_axis *= f32::from(size) / f32::from(SimulationOptions::LEGACY.terrain_size);
        }
        Self::generate(size, size, options.preset, options.seed, valley_axis)
    }

    pub(super) fn generate(w: u16, h: u16, preset: MapPreset, seed: u64, valley_axis: f32) -> Self {
        let mut me = Self {
            heightmap: Heightmap::new(w, h),
            water: WaterState::default(),
            base_revision: 0,
            trees: Grid::new(TREE_GRID_SIZE as i32),
        };
        // Bound the shift in noise space to keep enough precision for nearby cells.
        let noise_offset = if seed == 0 {
            Vec2::ZERO
        } else {
            let mut rng = RandProvider::new(seed);
            64.0 * vec2(rng.next_f32(), rng.next_f32())
        };
        for y in 0..h {
            let chunks: Vec<_> = (0..w)
                .into_par_iter()
                .map(|x| me.generate_chunk((x, y), preset, seed, noise_offset, valley_axis))
                .collect();
            for (x, (chunk, trees)) in (0..w).zip(chunks) {
                me.heightmap.set_chunk((x, y), chunk);
                for tree in trees {
                    me.trees.insert(tree.pos, tree);
                }
            }
        }
        me
    }

    /// Returns the height of the terrain at the given position in meters, capped at 0
    pub fn height(&self, pos: Vec2) -> Option<f32> {
        self.heightmap.height(pos).map(|x| x.max(0.0))
    }

    /// Returns the height of the terrain at the given position in meters, not capped at 0 (can be negative in water)
    pub fn true_height(&self, pos: Vec2) -> Option<f32> {
        self.heightmap.height(pos)
    }

    pub fn remove_trees_near(
        &mut self,
        obj: impl Intersect<Vec2>,
        mut f: impl FnMut(TerrainChunkID),
    ) {
        let mut to_remove = vec![];

        let bbox = obj.bbox();
        self.trees.query_aabb_visitor(bbox.ll, bbox.ur, |(h, pos)| {
            if obj.intersects(&pos) {
                to_remove.push(h);
            }
        });

        let mut seen = FastSet::new();
        for h in to_remove {
            let Some(tree) = self.trees.remove_maintain(h) else {
                continue;
            };
            let id = TerrainChunkID::new(tree.pos);
            if seen.insert(id) {
                f(id);
            }
        }
    }

    /// Filter generated vegetation before the imported map is first displayed.
    pub(crate) fn retain_imported_trees(&mut self, retain: impl FnMut(Vec2) -> bool) {
        Self::retain_trees(&mut self.trees, retain);
    }

    pub(super) fn remove_wet_generated_trees(&mut self) {
        let water = &self.water;
        let map = &self.heightmap;
        Self::retain_trees(&mut self.trees, |pos| !water.occupies(map, pos));
    }

    fn retain_trees(trees: &mut Grid<Tree, Vec2>, mut retain: impl FnMut(Vec2) -> bool) {
        let mut remove = Vec::new();
        let mut cells: Vec<_> = trees.storage().cells.iter().collect();
        cells.sort_unstable_by_key(|(&id, _)| id);
        for (_, cell) in cells {
            remove.extend(
                cell.objs
                    .iter()
                    .filter_map(|&(id, pos)| (!retain(pos)).then_some(id)),
            );
        }
        for id in remove {
            trees.remove_maintain(id);
        }
    }

    pub fn get_chunk(&self, id: TerrainChunkID) -> Option<&Chunk> {
        self.heightmap.get_chunk((id.0 as u16, id.1 as u16))
    }

    pub fn bounds(&self) -> AABB {
        self.heightmap.bounds()
    }

    /// Returns the size of the map in chunks
    pub fn size(&self) -> (u16, u16) {
        (self.heightmap.w, self.heightmap.h)
    }

    pub fn chunks(&self) -> impl Iterator<Item = (TerrainChunkID, &Chunk)> + '_ {
        self.heightmap
            .chunks()
            .map(|((x, y), c)| (TerrainChunkID::new_i16(x as i16, y as i16), c))
    }

    pub fn covered_chunks(&self, bounds: AABB) -> impl Iterator<Item = TerrainChunkID> {
        self.heightmap
            .covered_chunks(bounds)
            .map(|(x, y)| TerrainChunkID::new_i16(x as i16, y as i16))
    }

    pub fn raycast(&self, ray: Ray3) -> Option<(Vec3, Vec3)> {
        self.heightmap.raycast(ray)
    }

    pub fn set_overrides(
        &mut self,
        chunk: TerrainChunkID,
        overrides: [[u16; TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION],
    ) {
        self.heightmap
            .set_override((chunk.0 as u16, chunk.1 as u16), overrides);
    }

    /// Apply raw base writes for dry terrain generation and fixtures.
    pub fn terrain_apply(
        &mut self,
        bounds: AABB,
        f: impl FnMut(Vec3) -> f32,
    ) -> Vec<TerrainChunkID> {
        assert!(
            !self.has_water_configuration(),
            "configured water requires a staged base patch"
        );
        self.base_revision = self.base_revision.wrapping_add(1);
        self.heightmap
            .apply(bounds, f)
            .into_iter()
            .map(|(x, y)| TerrainChunkID::new_i16(x as i16, y as i16))
            .collect()
    }

    pub(super) fn terraform(
        &mut self,
        tick: Tick,
        kind: TerraformKind,
        center: Vec2,
        radius: f32,
        amount: f32,
        level: f32,
        slope: Option<(Vec3, Vec3)>,
    ) -> Vec<TerrainChunkID> {
        if self.has_water_configuration() {
            // The live map transaction validates occupancy against staged water.
            return Vec::new();
        }
        self.base_revision = self.base_revision.wrapping_add(1);
        let brush = TerraformBrush {
            kind,
            center,
            radius,
            amount,
            level,
            slope,
        };
        let bbox = AABB::centered(center, Vec2::splat(radius * 2.0));
        let modified = match kind {
            TerraformKind::Smooth => self
                .heightmap
                .apply_convolution(bbox, |pos, vals| brush.smooth(pos, vals)),
            TerraformKind::Erode => {
                let (particles, mut rng) = brush.erosion_particles(tick);
                self.heightmap.erode(bbox, particles, &mut rng)
            }
            _ => self.heightmap.apply(bbox, |pos| brush.height(pos)),
        };
        modified
            .into_iter()
            .map(|(x, y)| TerrainChunkID::new_i16(x as i16, y as i16))
            .collect()
    }

    /// Stage Terraform against packed base nodes, then prepare the resulting hydrology.
    pub(crate) fn prepare_water_terraform(
        &self,
        tick: Tick,
        kind: TerraformKind,
        center: Vec2,
        radius: f32,
        amount: f32,
        level: f32,
        slope: Option<(Vec3, Vec3)>,
    ) -> Result<PreparedWaterEdit, WaterEditError> {
        validate_map_dimensions(&self.heightmap)?;
        let brush = TerraformBrush {
            kind,
            center,
            radius,
            amount,
            level,
            slope,
        };
        brush.validate()?;
        let bbox = AABB::centered(center, Vec2::splat(radius * 2.0));
        let mut patch = BaseNodePatch::new(MAX_WATER_BASE_WRITES);
        let work = if kind == TerraformKind::Erode {
            let (particles, mut rng) = brush.erosion_particles(tick);
            let work = particles
                .checked_mul(Heightmap::EROSION_WORK_PER_PARTICLE)
                .filter(|&work| work <= MAX_WATER_BATCH_WORK)
                .ok_or(WaterEditError::Limit)?;
            self.heightmap
                .stage_base_erosion(&mut patch, bbox, particles, &mut rng)?;
            work
        } else {
            let r = TERRAIN_CHUNK_RESOLUTION;
            let work_per_node = if kind == TerraformKind::Smooth { 10 } else { 1 };
            let chunk_work = r * r * work_per_node;
            let work = self.heightmap.covered_chunks(bbox).count() * chunk_work;
            if work > MAX_WATER_BATCH_WORK {
                return Err(WaterEditError::Limit);
            }
            for (cx, cy) in self.heightmap.covered_chunks(bbox) {
                for y in usize::from(cy) * r..(usize::from(cy) + 1) * r {
                    for x in usize::from(cx) * r..(usize::from(cx) + 1) * r {
                        let point = vec2(x as f32, y as f32) * CELL_SIZE;
                        if !bbox.contains(point) {
                            continue;
                        }
                        let packed = self.heightmap.packed_base_node((x, y)).unwrap();
                        let current = unpack_height(packed);
                        let height = if kind == TerraformKind::Smooth {
                            let nodes = std::array::from_fn(|i| {
                                let node =
                                    ((x + i % 3).saturating_sub(1), (y + i / 3).saturating_sub(1));
                                self.heightmap
                                    .packed_base_node(node)
                                    .map(unpack_height)
                                    .unwrap_or(0.0)
                            });
                            brush.smooth(point, nodes)
                        } else {
                            brush.height(point.z(current))
                        };
                        if !height.is_finite() {
                            return Err(WaterEditError::InvalidInput);
                        }
                        if height != current {
                            patch.stage(&self.heightmap, (x, y), pack_height(height))?;
                        }
                    }
                }
            }
            work
        };
        self.prepare_water_base_patch_with_work(patch, work)
    }

    fn generate_chunk(
        &self,
        (x, y): (u16, u16),
        preset: MapPreset,
        seed: u64,
        noise_offset: Vec2,
        valley_axis: f32,
    ) -> (Chunk, Vec<Tree>) {
        let pchunk = TerrainChunkID::SIZE_F32 * vec2(x as f32, y as f32);
        let mut heights = [[pack_height(10.0); TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION];
        if preset == MapPreset::Procedural {
            for (y, line) in heights.iter_mut().enumerate() {
                for (x, height) in line.iter_mut().enumerate() {
                    let offcell = vec2(x as f32, y as f32) * CELL_SIZE;
                    let mut relief =
                        heightmap::height(pchunk + offcell, noise_offset, valley_axis).0 - 0.12;
                    if relief > 0.0 {
                        relief = 0.0;
                    }
                    *height = pack_height(1000.0 * relief);
                }
            }
        }
        let chunk = Chunk::new(heights);

        let rchunk = if seed == 0 {
            common::rand::rand2(x as f32, y as f32)
        } else {
            common::rand::randhash((seed, x, y))
        };

        const RES_TREES: usize = 64;
        const TCELLW: f32 = TerrainChunkID::SIZE_F32 / RES_TREES as f32;

        let mut trees = Vec::with_capacity(128);

        let tree_storage = self.trees.storage();

        for offx in 0..RES_TREES {
            for offy in 0..RES_TREES {
                let cellpos = vec2(offx as f32, offy as f32) * TCELLW;

                let rcell = common::rand::rand2(cellpos.x, cellpos.y);
                let jitterx = common::rand::rand3(rchunk, rcell, 1.0);
                let jittery = common::rand::rand3(rchunk, rcell, 2.0);
                let dens_test = common::rand::rand3(rchunk, rcell, 3.0);

                let sample = cellpos + vec2(jitterx, jittery) * TCELLW;

                let tdens = tree_density(pchunk + sample);

                if dens_test < tdens && chunk.height_unchecked(sample) >= 0.0 {
                    let pos = pchunk + sample;
                    // normalize pos
                    let cell = tree_storage.cell_id(pos);
                    let pos = decode_pos(encode_pos(pos, cell), cell);
                    trees.push(Tree::new(pos));
                }
            }
        }

        (chunk, trees)
    }
}

struct TerraformBrush {
    kind: TerraformKind,
    center: Vec2,
    radius: f32,
    amount: f32,
    level: f32,
    slope: Option<(Vec3, Vec3)>,
}

impl TerraformBrush {
    fn validate(&self) -> Result<(), WaterEditError> {
        if !self.center.is_finite()
            || !(self.radius > 0.0 && self.radius <= 2_000.0)
            || !(-2_000.0..=2_000.0).contains(&self.amount)
            || !self.level.is_finite()
            || self.slope.is_some_and(|(start, end)| {
                let length = start.xy().distance2(end.xy());
                !start.is_finite() || !end.is_finite() || !length.is_finite() || length == 0.0
            })
        {
            return Err(WaterEditError::InvalidInput);
        }
        Ok(())
    }

    fn weight(&self, pos: Vec2) -> f32 {
        let dist = pos.distance(self.center) / self.radius;
        if dist >= 1.0 {
            0.0
        } else {
            (-1.0 / (1.0 - dist * dist)).exp()
        }
    }

    fn height(&self, pos: Vec3) -> f32 {
        let weight = self.weight(pos.xy());
        if weight == 0.0 {
            return pos.z;
        }
        let delta = (self.amount * DELTA) * weight;
        let target = match self.kind {
            TerraformKind::Elevation => return pos.z + delta,
            TerraformKind::Level => self.level,
            TerraformKind::Slope => {
                let Some((p1, p2)) = self.slope else {
                    return pos.z;
                };
                let d = p2.xy() - p1.xy();
                let along = (pos.xy() - p1.xy()).dot(d) / d.mag2();
                lerp(p1.z, p2.z, along.clamp(0.0, 1.0))
            }
            TerraformKind::Smooth | TerraformKind::Erode => unreachable!(),
        };
        pos.z + delta * (target - pos.z).signum() * (target - pos.z).abs().mul(0.1).clamp(0.0, 1.0)
    }

    fn smooth(&self, pos: Vec2, vals: [f32; 9]) -> f32 {
        let weight = self.weight(pos);
        if weight == 0.0 {
            return vals[4];
        }
        const GAUSSIAN_KERNEL: [f32; 9] = [
            0.07511361, 0.1238414, 0.07511361, 0.1238414, 0.20417996, 0.1238414, 0.07511361,
            0.1238414, 0.07511361,
        ];
        let sum = vals
            .into_iter()
            .zip(GAUSSIAN_KERNEL)
            .map(|(a, b)| a * b)
            .sum::<f32>();
        vals[4] + weight * (sum - vals[4])
    }

    fn erosion_particles(&self, tick: Tick) -> (usize, impl FnMut() -> f32) {
        let mut rng = common::rand::gen(tick.0);
        let count = self.amount * DELTA * self.radius * self.radius * 0.00002;
        let mut particles = count as usize;
        if count.fract() > rng.next_f32() {
            particles += 1;
        }
        (particles, move || rng.next_f32())
    }
}

impl Tree {
    pub fn new(pos: Vec2) -> Self {
        let crand = common::rand::rand3(pos.x, pos.y, 1.0);

        let colscale = 0.7 - 0.2 * crand;
        let angle = Radians(2.0 * std::f32::consts::PI * common::rand::rand3(pos.x, pos.y, 2.0));

        let srand = common::rand::rand3(pos.x, pos.y, 3.0);
        let scale = 5.0 + 3.0 * srand;

        Tree {
            pos,
            size: scale,
            col: colscale,
            dir: angle.vec2(),
        }
    }
}

type SmolTree = u16;

pub fn encode_pos(pos: Vec2, chunk: CellIdx) -> SmolTree {
    let diffx = pos.x - (chunk.0 * TREE_GRID_SIZE as i32) as f32;
    let diffy = pos.y - (chunk.1 * TREE_GRID_SIZE as i32) as f32;

    ((((diffx / TREE_GRID_SIZE as f32) * 256.0) as u8 as u16) << 8)
        + ((diffy / TREE_GRID_SIZE as f32) * 256.0) as u8 as u16
}

pub fn decode_pos(encoded: SmolTree, chunk: CellIdx) -> Vec2 {
    let diffx = (encoded >> 8) as u8;
    let diffy = (encoded & 0xFF) as u8;
    Vec2 {
        x: TREE_GRID_SIZE as f32 * (chunk.0 as f32 + diffx as f32 / 256.0),
        y: TREE_GRID_SIZE as f32 * (chunk.1 as f32 + diffy as f32 / 256.0),
    }
}

#[derive(Serialize, Deserialize)]
struct SerializedEnvironment<H, W> {
    h: H,
    trees: Vec<(CellIdx, Vec<SmolTree>)>,
    water: W,
}

struct CheckedHeightmap(Heightmap);

impl<'de> Deserialize<'de> for CheckedHeightmap {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Heightmap::deserialize_bounded::<D, MAX_WATER_CHUNKS_PER_AXIS>(deserializer).map(Self)
    }
}

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let SerializedEnvironment {
            h: CheckedHeightmap(heightmap),
            trees,
            mut water,
        } = SerializedEnvironment::<CheckedHeightmap, WaterState>::deserialize(deserializer)?;
        water
            .validate_and_rebuild(&heightmap)
            .map_err(serde::de::Error::custom)?;
        let mut terrain = Self {
            heightmap,
            water,
            base_revision: 0,
            trees: Grid::new(TREE_GRID_SIZE as i32),
        };

        for (chunk_id, trees) in trees {
            for tree in trees {
                let tree = Tree::new(decode_pos(tree, chunk_id));
                terrain.trees.insert(tree.pos, tree);
            }
        }
        Ok(terrain)
    }
}

impl Serialize for Environment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let tree_cells = &self.trees.storage().cells;

        let mut keys = tree_cells.keys().copied().collect::<Vec<_>>();
        keys.sort_unstable();

        let mut trees = Vec::new();
        for cell_id in keys {
            let chunk = &tree_cells[&cell_id];
            // Removing trees can leave an empty cell that loading never recreates.
            if chunk.objs.is_empty() {
                continue;
            }
            let mut smoltrees = Vec::with_capacity(chunk.objs.len());
            for (_, tree_pos) in chunk.objs.iter() {
                let smol = encode_pos(*tree_pos, cell_id);
                smoltrees.push(smol);
            }
            trees.push((cell_id, smoltrees));
        }

        SerializedEnvironment {
            h: &self.heightmap,
            trees,
            water: &self.water,
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod water_w1_tests {
    use super::*;
    use crate::map::{RiverBank, WaterBodyId, WaterEdit, WaterKind, WaterTileCoord};
    use common::saveload::{Bincode, Encoder, JSON};
    use geom::{vec3, Polygon};

    fn flat(w: u16, h: u16) -> Environment {
        Environment::generate(w, h, MapPreset::Flat, 0, 0.0)
    }

    #[test]
    fn w1_water_raycast_keeps_the_half_open_owner() {
        let mut env = flat(2, 1);
        for edge in [32.0, 512.0, 1024.0] {
            env.fill_lake(
                &Polygon::centered_rect(vec2(edge - 16.0, 16.0), 32.0, 32.0),
                0,
                2.0,
            )
            .unwrap();
            for ray in [
                Ray3::new(
                    vec3(edge - 1.0, 8.0, 1.0),
                    vec3(1.0f32.next_down(), 0.0, -1.0),
                ),
                Ray3::new(vec3(edge, 8.0, 1.0), vec3(-f32::EPSILON / 2.0, 0.0, -1.0)),
            ] {
                let hit = env.raycast_water(ray, 1.0).unwrap();
                assert!(hit.point.x < edge, "{hit:?}");
                assert_eq!(env.water_at(hit.point.xy()), Some(hit.sample));
                assert_eq!(hit.t, 1.0);
            }
            let boundary = Ray3::new(vec3(edge, 8.0, 1.0), -Vec3::Z);
            assert!(env.raycast_water(boundary, 1.0).is_none());
        }
    }

    #[test]
    fn w1_water_sea_preparation_charges_contacts_and_stages_banks() {
        let mut env = flat(3, 2);
        env.terrain_apply(env.bounds(), |_| -4.0);
        let id = env
            .carve_river_at_level(
                &[vec2(464.0, 520.0), vec2(560.0, 520.0)],
                32.0,
                2.0,
                Some(0),
            )
            .unwrap();
        let bank = env.river_bank_near(id, 48, RiverBank::Left, 32).unwrap();
        assert!(env
            .prepare_water_edit(&WaterEdit::SetSeaLevel { level_dm: Some(1) })
            .is_err());
        let prepared = env
            .prepare_water_edit(&WaterEdit::SetSeaLevel { level_dm: Some(0) })
            .unwrap();
        assert!(
            prepared.work() > 0 && prepared.work() < 2048,
            "{}",
            prepared.work()
        );
        assert_eq!(env.river_bank_near(id, 48, RiverBank::Left, 32), Some(bank));
        env.commit_prepared_water(prepared).unwrap();
        assert!(env.river_bank_near(id, 48, RiverBank::Left, 32).is_none());
        env.set_sea_level(None).unwrap();
        assert_eq!(env.river_bank_near(id, 48, RiverBank::Left, 32), Some(bank));
    }

    #[test]
    fn w1_water_slot_rejects_mismatched_heightmap_chunk_count() {
        use crate::{
            SaveHeader, SaveLoadError, Simulation, SimulationDeser, SimulationSer,
            SlotSnapshotHeader,
        };
        use common::saveload::{slot_paths, SlotId, SnapshotRevision};

        let ctx = crate::tests::TestCtx::new();
        let mut data: SimulationDeser = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
        let map = data.res.get_mut("map").unwrap();
        let heightmap = &ctx.g.map().environment.heightmap;
        let original = Bincode::encode(heightmap).unwrap();
        let chunks = heightmap
            .chunks()
            .map(|(_, chunk)| chunk)
            .collect::<Vec<_>>();
        assert_eq!(chunks.len(), 1);
        let malformed = Bincode::encode(&(chunks, 2u16, 1u16)).unwrap();
        let offset = map
            .windows(original.len())
            .position(|window| window == original)
            .unwrap();
        map.splice(offset..offset + original.len(), malformed);

        let header = SlotSnapshotHeader {
            magic: crate::SLOT_SNAPSHOT_MAGIC,
            version: crate::VERSION.into(),
            revision: SnapshotRevision::new(1),
        };
        let payload = SimulationSer {
            header: SaveHeader {
                version: crate::VERSION.into(),
            },
            world: &data.world,
            res: data.res,
        };
        let root = std::env::temp_dir().join(format!("egregoria-w1-slot-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let id = SlotId::new("bad-heightmap").unwrap();
        let path = slot_paths(&root, &id).unwrap().bc;
        std::fs::write(path, Bincode::encode(&(header, payload)).unwrap()).unwrap();
        let result = Simulation::load_from_slot(&root, &id);
        std::fs::remove_dir_all(&root).unwrap();
        let Err(SaveLoadError::Corrupt {
            version: Some(version),
            reason,
        }) = result
        else {
            panic!("expected a corrupt current-version snapshot");
        };
        assert_eq!(version, crate::VERSION);
        assert!(reason.contains("resource map"), "{reason}");
        assert!(
            reason.contains("heightmap chunk count mismatch: expected 2, got 1"),
            "{reason}"
        );
    }

    #[test]
    fn w1_water_coast_lake_river_and_roundtrip() {
        let mut env = flat(3, 2);
        env.terrain_apply(env.bounds(), |p| if p.x < 128.0 { -6.0 } else { p.z });
        let lake = Polygon::centered_rect(vec2(600.0, 750.0), 32.0, 32.0);
        let lake_id = env.fill_lake(&lake, 0, 2.0).unwrap();
        assert_eq!(
            env.water_at(vec2(600.0, 750.0)).unwrap().kind,
            WaterKind::Lake
        );
        env.set_sea_level(Some(0)).unwrap();
        assert_eq!(
            env.water_at(vec2(1.0, 120.0)).unwrap().body,
            WaterBodyId::SEA
        );
        assert!(!env.is_water(vec2(112.0, 120.0)));
        let stroke = [vec2(900.0, 512.0), vec2(0.0, 512.0)];
        let id = env
            .carve_river_at_level(&stroke, 48.0, 3.0, Some(0))
            .unwrap();
        let sample = env.water_at(vec2(520.0, 512.0)).unwrap();
        assert_eq!(sample.body, id);
        assert_eq!(sample.reach_m, Some(380));
        let bank = env
            .river_bank_near(id, 380, crate::map::RiverBank::Left, 16)
            .unwrap();
        assert!(bank.point.y < 512.0);
        assert!(env.shoreline_distance(vec2(520.0, 512.0), 80.0).unwrap() > 0.0);
        let ray = Ray3::new(vec3(520.0, 512.0, 20.0), -Vec3::Z);
        assert_eq!(env.raycast_water(ray, 20.0).unwrap().sample.body, id);
        assert!(!env.is_water(vec2(1536.0, 512.0)));

        let bytes = Bincode::encode(&env).unwrap();
        let mut loaded: Environment = Bincode::decode(&bytes).unwrap();
        assert_eq!(Bincode::encode(&loaded).unwrap(), bytes);
        let readable: Environment = JSON::decode(&JSON::encode(&env).unwrap()).unwrap();
        assert_eq!(readable.water_revision(), env.water_revision());
        assert_eq!(readable.water_at(vec2(600.0, 750.0)).unwrap().body, lake_id);
        loaded.remove_freshwater(id).unwrap();
        assert_eq!(
            loaded.water_at(vec2(520.0, 512.0)).unwrap().kind,
            WaterKind::Sea
        );
        loaded.set_sea_level(None).unwrap();
        assert!(!loaded.is_water(vec2(520.0, 512.0)));
        assert_eq!(loaded.height(vec2(520.0, 512.0)), Some(0.0));
        assert!(loaded.strict_water_siting());
        assert!(loaded.is_water(vec2(600.0, 750.0)));
    }

    #[test]
    fn w1_water_prospective_sill_flood_and_packed_noop() {
        let mut env = flat(3, 1);
        env.terrain_apply(env.bounds(), |p| {
            if p.x > 0.0 && p.x <= 1408.0 && (96.0..=144.0).contains(&p.y) {
                -3.0
            } else {
                p.z
            }
        });
        env.set_sea_level(Some(0)).unwrap();
        let remote = vec2(1300.0, 108.0);
        assert!(env.base_height(remote).unwrap() < 0.0);
        assert!(!env.is_water(remote));
        assert!(!env.strict_water_siting());
        let before = Bincode::encode(&env).unwrap();
        let mut patch = BaseNodePatch::new(MAX_WATER_BASE_WRITES);
        for y in 6..=7 {
            patch
                .stage(&env.heightmap, (0, y), pack_height(-3.0))
                .unwrap();
        }
        let prepared = env.prepare_water_base_patch(patch).unwrap();
        assert!(prepared.strict_water_siting());
        assert!(prepared.water_at(&env, remote).is_some());
        assert!(prepared
            .surface_changed_tiles()
            .any(|t| t == WaterTileCoord(81, 6)));
        assert!(prepared.affected_tiles().any(|t| t == WaterTileCoord(0, 5)));
        assert!(prepared
            .dirty()
            .water()
            .contains(&TerrainChunkID::new_i16(2, 0)));
        assert!(!prepared
            .dirty()
            .terrain()
            .contains(&TerrainChunkID::new_i16(2, 0)));
        assert_eq!(Bincode::encode(&env).unwrap(), before);
        env.commit_prepared_water(prepared).unwrap();
        assert!(env.is_water(remote));
        assert!(env.strict_water_siting());

        let revision = env.water_revision();
        let noop = env
            .prepare_water_terraform(
                Tick(2),
                TerraformKind::Elevation,
                vec2(512.0, 108.0),
                32.0,
                0.0,
                0.0,
                None,
            )
            .unwrap();
        assert!(noop.affected_tiles().next().is_none());
        assert!(noop.work() > 0);
        assert!(!env.commit_prepared_water(noop).unwrap().changed);
        let change = env.set_sea_level(Some(5)).unwrap();
        assert!(change.changed);
        assert_eq!(env.sea_level_dm(), Some(5));
        assert_eq!(env.water_revision(), revision + 1);
        assert!(!change.dirty.water().is_empty());
        assert!(change.dirty.terrain().is_empty());
        assert!(!change.dirty.road_support().is_empty());
        let retry = env.set_sea_level(Some(5)).unwrap();
        assert!(!retry.changed);
        assert_eq!(retry.dirty, Default::default());
        assert_eq!(env.water_revision(), revision + 1);
    }

    #[test]
    fn w1_water_citymap_edges_and_atomic_bed_mouth_checks() {
        let mut env = flat(5, 5);
        let river = [
            vec2(1256.0, 0.0),
            vec2(1296.0, 640.0),
            vec2(1248.0, 1280.0),
            vec2(1296.0, 1920.0),
            vec2(1264.0, 2560.0),
        ];
        let id = env.carve_river(&river, 96.0, 5.0).unwrap();
        assert_eq!(env.river_centerline(id), Some(river.as_slice()));
        assert_eq!(env.water_surface_elevation(vec2(1264.0, 2559.0)), Some(9.0));
        assert!(!env.is_water(river[4]));
        let left = env.river_bank_near(id, 1280, RiverBank::Left, 128).unwrap();
        let right = env
            .river_bank_near(id, 1280, RiverBank::Right, 128)
            .unwrap();
        assert!(left.point.x < 1248.0 && right.point.x > 1248.0);
        assert!(env.is_wet_base_node((79, 159)));
        let seam_tiles = env
            .water_tiles(TerrainChunkID::new_i16(2, 4))
            .collect::<Vec<_>>();
        assert!(seam_tiles
            .windows(2)
            .all(|pair| pair[0].tile.local_index() < pair[1].tile.local_index()));
        let crossing = Polygon::centered_rect(vec2(1264.0, 2552.0), 256.0, 1.0);
        let boundary = Polygon::centered_rect(right.dry_tile.center(), 16.0, 16.0);
        assert!(env.intersects_water(&crossing));
        assert!(!env.intersects_water(&boundary));

        let bytes = Bincode::encode(&env).unwrap();
        let conflict = env.set_sea_level(Some(110)).unwrap_err();
        assert!(matches!(conflict, WaterEditError::IncompatibleMouth(_, _)));
        let mut raised = BaseNodePatch::new(MAX_WATER_BASE_WRITES);
        raised
            .stage(&env.heightmap, (79, 159), pack_height(10.0))
            .unwrap();
        assert!(matches!(
            env.prepare_water_base_patch(raised),
            Err(WaterEditError::InvalidBed(_))
        ));
        assert_eq!(Bincode::encode(&env).unwrap(), bytes);

        let stale = env
            .prepare_water_edit(&WaterEdit::RemoveFreshWater { body: id })
            .unwrap();
        env.fill_lake(
            &Polygon::centered_rect(vec2(2000.0, 1000.0), 48.0, 64.0),
            0,
            3.0,
        )
        .unwrap();
        assert!(matches!(
            env.commit_prepared_water(stale),
            Err(WaterEditError::Stale)
        ));
    }

    #[test]
    fn w1_water_freshwater_blocks_sea_and_ids_stay_exhausted_after_reload() {
        let mut env = flat(2, 1);
        env.terrain_apply(env.bounds(), |p| {
            if (96.0..=128.0).contains(&p.y) && p.x < 896.0 {
                -4.0
            } else {
                p.z
            }
        });
        env.water.next_freshwater_id = u32::from(u16::MAX);
        env.water.strict = true;
        env.water.revision = 2 * u64::from(u16::MAX - 1);
        let blocker = Polygon::centered_rect(vec2(264.0, 112.0), 16.0, 32.0);
        let id = env.fill_lake(&blocker, 0, 2.0).unwrap();
        assert_eq!(id, WaterBodyId(u16::MAX));
        env.set_sea_level(Some(0)).unwrap();
        assert_eq!(
            env.water_at(vec2(128.0, 112.0)).unwrap().kind,
            WaterKind::Sea
        );
        assert!(!env.is_water(vec2(800.0, 112.0)));
        env.remove_freshwater(id).unwrap();
        let bytes = Bincode::encode(&env).unwrap();
        let mut env: Environment = Bincode::decode(&bytes).unwrap();
        assert_eq!(
            env.water_at(vec2(800.0, 112.0)).unwrap().kind,
            WaterKind::Sea
        );
        assert_eq!(
            env.fill_lake(&blocker, 0, 2.0),
            Err(WaterEditError::BodyIdExhausted)
        );
        assert_eq!(Bincode::encode(&env).unwrap(), bytes);
    }
}

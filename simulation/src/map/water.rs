//! Static water authority, staged base/coverage edits, and shared tile queries.

use super::terrain::{Environment, Heightmap, TerrainChunkID, CELL_SIZE, TERRAIN_CHUNK_RESOLUTION};
use super::ProjectKind;
use geom::{
    pack_base_height_at_most, vec2, BaseNode, BaseNodePatch, BasePatchError, Polygon, Ray3, Vec2,
    Vec2d, Vec3,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

pub const MAX_WATER_CHUNKS_PER_AXIS: u16 = 80;
pub const MAX_WATER_VERTICES: usize = 256;
pub const MAX_WATER_EDIT_BYTES: usize = 8 * 1024;
// A binary Vec2 is a packed u64. Both its varint and readable integer
// formats, plus the fixed command fields, fit in the edit envelope.
const _: () = assert!(MAX_WATER_VERTICES * 22 + 256 <= MAX_WATER_EDIT_BYTES);
pub const MAX_FRESHWATER_CANDIDATES: usize = 65_536;
pub const MAX_WATER_TILE_TESTS: usize = 2_097_152;
pub const MAX_WATER_BASE_WRITES: usize = 131_072;
pub const MAX_WATER_BATCH_WORK: usize = 4_194_304;
pub const MAX_WATER_RIVER_VERTICES: usize = 262_144;
pub(super) const MAX_FRESHWATER_BODIES: usize = u16::MAX as usize;
pub(super) const MAX_WATER_CHUNKS: usize = (MAX_WATER_CHUNKS_PER_AXIS as usize).pow(2);
pub(super) const TILES_PER_CHUNK: usize = TERRAIN_CHUNK_RESOLUTION.pow(2);
pub(super) const MAX_FRESHWATER_CELLS: usize = MAX_WATER_CHUNKS * TILES_PER_CHUNK;
const TERMINAL_BODY_ID: u32 = u16::MAX as u32 + 1;
pub(super) const MIN_DEPTH: f64 = 0.25;
const PITCH: f64 = CELL_SIZE as f64;
const RES: i16 = TERRAIN_CHUNK_RESOLUTION as i16;
const DIRECTIONS: [(i16, i16); 4] = [(-1, 0), (0, -1), (0, 1), (1, 0)];

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WaterBodyId(pub u16);

impl WaterBodyId {
    pub const SEA: Self = Self(0);
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaterKind {
    Sea,
    River,
    Lake,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RiverBank {
    Left,
    Right,
}

impl RiverBank {
    fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
}

/// An in-map tile coordinate, at the 16 m pitch.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WaterTileCoord(pub i16, pub i16);

impl WaterTileCoord {
    pub fn center(self) -> Vec2 {
        self.corner() + Vec2::splat(CELL_SIZE * 0.5)
    }

    pub fn corner(self) -> Vec2 {
        vec2(f32::from(self.0), f32::from(self.1)) * CELL_SIZE
    }

    pub fn chunk(self) -> TerrainChunkID {
        TerrainChunkID::new_i16(self.0.div_euclid(RES), self.1.div_euclid(RES))
    }

    pub fn local_index(self) -> usize {
        (self.0.rem_euclid(RES) + RES * self.1.rem_euclid(RES)) as usize
    }

    fn from_local(chunk: TerrainChunkID, index: usize) -> Self {
        Self(
            chunk.0 * RES + index as i16 % RES,
            chunk.1 * RES + index as i16 / RES,
        )
    }

    fn neighbor(self, direction: usize) -> Self {
        let (dx, dy) = DIRECTIONS[direction];
        Self(self.0 + dx, self.1 + dy)
    }

    fn edge_point(self, direction: usize) -> Vec2 {
        (self.center() + self.neighbor(direction).center()) * 0.5
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct WaterSample {
    pub body: WaterBodyId,
    pub kind: WaterKind,
    pub surface_z: f32,
    pub depth_m: f32,
    pub reach_m: Option<u16>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct WaterTile {
    pub tile: WaterTileCoord,
    pub sample: WaterSample,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ShoreContact {
    pub body: WaterBodyId,
    pub kind: WaterKind,
    pub surface_z: f32,
    pub point: Vec2,
    pub dry_tile: WaterTileCoord,
    pub wet_tile: WaterTileCoord,
    pub reach_m: Option<u16>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct WaterHit {
    pub point: Vec3,
    pub t: f32,
    pub sample: WaterSample,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WaterStroke(Vec<Vec2>);

impl WaterStroke {
    pub fn new(points: &[Vec2]) -> Result<Self, WaterEditError> {
        validate_shape(points, false)?;
        river_length(points)?;
        Ok(Self(points.to_vec()))
    }

    pub fn points(&self) -> &[Vec2] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WaterBasin(Vec<Vec2>);

impl WaterBasin {
    pub fn new(points: &[Vec2]) -> Result<Self, WaterEditError> {
        validate_shape(points, true)?;
        Ok(Self(points.to_vec()))
    }

    pub fn points(&self) -> &[Vec2] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WaterEdit {
    SetSeaLevel {
        #[serde(deserialize_with = "super::water_serde::optional_level")]
        level_dm: Option<i16>,
    },
    CarveRiver {
        polyline: WaterStroke,
        #[serde(deserialize_with = "super::water_serde::river_width")]
        width_m: f32,
        #[serde(deserialize_with = "super::water_serde::depth")]
        depth_m: f32,
        #[serde(deserialize_with = "super::water_serde::optional_level")]
        level_dm: Option<i16>,
    },
    FillLake {
        basin: WaterBasin,
        #[serde(deserialize_with = "super::water_serde::level")]
        level_dm: i16,
        #[serde(deserialize_with = "super::water_serde::depth")]
        depth_m: f32,
    },
    RemoveFreshWater {
        body: WaterBodyId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaterEditError {
    InvalidInput,
    OutOfBounds,
    Limit,
    BodyUnavailable(WaterBodyId),
    BodyIdExhausted,
    Disconnected(WaterTileCoord),
    Overlap(WaterTileCoord),
    IncompatibleMouth(WaterTileCoord, WaterTileCoord),
    InvalidBed(WaterTileCoord),
    OccupiedFootprint {
        object: ProjectKind,
        tile: WaterTileCoord,
    },
    TopologyConflict {
        object: ProjectKind,
        tile: WaterTileCoord,
    },
    InsufficientClearance {
        object: ProjectKind,
        tile: WaterTileCoord,
    },
    UnrepresentableLevel,
    Stale,
}

impl fmt::Display for WaterEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => f.write_str("invalid water level, depth or shape"),
            Self::OutOfBounds => f.write_str("water coordinates are outside the map"),
            Self::Limit => f.write_str("water work or storage limit exceeded"),
            Self::BodyUnavailable(id) => write!(f, "freshwater body {} is unavailable", id.0),
            Self::BodyIdExhausted => f.write_str("freshwater body IDs are exhausted"),
            Self::Disconnected(tile) => write!(f, "disconnected water at tile {tile:?}"),
            Self::Overlap(tile) => write!(f, "freshwater overlap at tile {tile:?}"),
            Self::IncompatibleMouth(a, b) => {
                write!(f, "unequal water surfaces at tiles {a:?} and {b:?}")
            }
            Self::InvalidBed(tile) => write!(f, "water bed is too shallow at tile {tile:?}"),
            Self::OccupiedFootprint { object, tile } => {
                write!(f, "{object:?} occupies wet tile {tile:?}")
            }
            Self::TopologyConflict { object, tile } => {
                write!(f, "{object:?} has invalid geometry at tile {tile:?}")
            }
            Self::InsufficientClearance { object, tile } => write!(
                f,
                "{object:?} has insufficient water clearance at tile {tile:?}"
            ),
            Self::UnrepresentableLevel => {
                f.write_str("water bed or surface is outside the supported elevation range")
            }
            Self::Stale => f.write_str("prepared water edit is stale"),
        }
    }
}

impl std::error::Error for WaterEditError {}

impl From<BasePatchError> for WaterEditError {
    fn from(error: BasePatchError) -> Self {
        match error {
            BasePatchError::Bounds => Self::OutOfBounds,
            BasePatchError::Limit => Self::Limit,
            BasePatchError::Stale => Self::Stale,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaterDirtyChunks {
    water: BTreeSet<TerrainChunkID>,
    terrain: BTreeSet<TerrainChunkID>,
    road_support: BTreeSet<TerrainChunkID>,
}

impl WaterDirtyChunks {
    pub fn new(
        water: BTreeSet<TerrainChunkID>,
        terrain: BTreeSet<TerrainChunkID>,
        road_support: BTreeSet<TerrainChunkID>,
    ) -> Self {
        Self {
            water,
            terrain,
            road_support,
        }
    }

    pub fn union(mut self, other: Self) -> Self {
        self.water.extend(other.water);
        self.terrain.extend(other.terrain);
        self.road_support.extend(other.road_support);
        self
    }

    pub fn water(&self) -> &BTreeSet<TerrainChunkID> {
        &self.water
    }

    pub fn terrain(&self) -> &BTreeSet<TerrainChunkID> {
        &self.terrain
    }

    pub fn road_support(&self) -> &BTreeSet<TerrainChunkID> {
        &self.road_support
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaterEditOutcome {
    pub changed: bool,
    pub body: Option<WaterBodyId>,
    pub dirty: WaterDirtyChunks,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct TileBounds {
    w: i16,
    h: i16,
}

impl TileBounds {
    fn new(map: &Heightmap) -> Self {
        Self {
            w: (map.w.min(MAX_WATER_CHUNKS_PER_AXIS) as i16) * RES,
            h: (map.h.min(MAX_WATER_CHUNKS_PER_AXIS) as i16) * RES,
        }
    }

    fn contains(self, WaterTileCoord(x, y): WaterTileCoord) -> bool {
        x >= 0 && x < self.w && y >= 0 && y < self.h
    }

    fn contains_chunk(self, id: TerrainChunkID) -> bool {
        id.0 >= 0 && id.0 < self.w / RES && id.1 >= 0 && id.1 < self.h / RES
    }

    fn index(self, chunk: TerrainChunkID) -> usize {
        chunk.1 as usize * (self.w / RES) as usize + chunk.0 as usize
    }

    fn chunk(self, index: usize) -> TerrainChunkID {
        let w = (self.w / RES) as usize;
        TerrainChunkID::new_i16((index % w) as i16, (index / w) as i16)
    }

    fn chunks(self) -> usize {
        (self.w / RES) as usize * (self.h / RES) as usize
    }

    fn position(self, x: f64, y: f64) -> Option<WaterTileCoord> {
        if !(x >= 0.0 && y >= 0.0 && x < f64::from(self.w) * PITCH && y < f64::from(self.h) * PITCH)
        {
            return None;
        }
        Some(WaterTileCoord((x / PITCH) as i16, (y / PITCH) as i16))
    }

    fn span(self, min: Vec2d, max: Vec2d) -> impl Iterator<Item = WaterTileCoord> {
        let (x0, x1) = span_axis(min.x, max.x, self.w);
        let (y0, y1) = span_axis(min.y, max.y, self.h);
        (y0..y1).flat_map(move |y| (x0..x1).map(move |x| WaterTileCoord(x, y)))
    }

    fn validate_vertices(self, points: &[Vec2]) -> Result<(), WaterEditError> {
        let upper = vec2(f32::from(self.w), f32::from(self.h)) * CELL_SIZE;
        if points
            .iter()
            .any(|p| p.x < 0.0 || p.y < 0.0 || p.x > upper.x || p.y > upper.y)
        {
            return Err(WaterEditError::OutOfBounds);
        }
        Ok(())
    }
}

fn span_axis(min: f64, max: f64, size: i16) -> (i16, i16) {
    let limit = f64::from(size);
    (
        (min / PITCH).floor().clamp(0.0, limit) as i16,
        (max / PITCH).ceil().clamp(0.0, limit) as i16,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TileMask([u64; TILES_PER_CHUNK / 64]);

impl Default for TileMask {
    fn default() -> Self {
        Self([0; TILES_PER_CHUNK / 64])
    }
}

impl TileMask {
    fn insert(&mut self, bit: usize) {
        self.0[bit / 64] |= 1 << (bit % 64);
    }

    fn contains(&self, bit: usize) -> bool {
        self.0[bit / 64] & (1 << (bit % 64)) != 0
    }

    fn is_empty(&self) -> bool {
        self.0.iter().all(|&word| word == 0)
    }

    fn union(&mut self, other: &Self) {
        for (word, rhs) in self.0.iter_mut().zip(other.0) {
            *word |= rhs;
        }
    }

    fn diff(self, other: Self, same_level: bool) -> Self {
        Self(std::array::from_fn(|i| {
            if same_level {
                self.0[i] ^ other.0[i]
            } else {
                self.0[i] | other.0[i]
            }
        }))
    }

    fn indices(self) -> impl Iterator<Item = usize> {
        self.0.into_iter().enumerate().flat_map(|(word, mut bits)| {
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(word * 64 + bit)
            })
        })
    }
}

#[derive(Default)]
struct TileSet(BTreeMap<TerrainChunkID, TileMask>);

impl TileSet {
    fn insert(&mut self, tile: WaterTileCoord) {
        self.0
            .entry(tile.chunk())
            .or_default()
            .insert(tile.local_index());
    }

    fn union_mask(&mut self, chunk: TerrainChunkID, mask: TileMask) {
        if !mask.is_empty() {
            self.0.entry(chunk).or_default().union(&mask);
        }
    }

    fn tiles(&self) -> impl Iterator<Item = WaterTileCoord> + '_ {
        self.0.iter().flat_map(|(&chunk, &mask)| {
            mask.indices()
                .map(move |index| WaterTileCoord::from_local(chunk, index))
        })
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FreshCell {
    pub(super) body: u16,
    pub(super) reach: u16,
}

type FreshChunk = [FreshCell; TILES_PER_CHUNK];

#[derive(Clone, Serialize, Deserialize)]
pub(super) enum BodyShape {
    Lake,
    River {
        stroke: WaterStroke,
        #[serde(deserialize_with = "super::water_serde::river_width")]
        width_m: f32,
    },
}

#[derive(Clone)]
pub(super) struct FreshBody {
    pub(super) level_dm: i16,
    pub(super) shape: BodyShape,
    pub(super) chunks: BTreeSet<TerrainChunkID>,
    pub(super) cells: usize,
    river: Option<RiverGeometry>,
}

impl FreshBody {
    pub(super) fn new(level_dm: i16, shape: BodyShape) -> Self {
        let river = match &shape {
            BodyShape::Lake => None,
            BodyShape::River { stroke, .. } => Some(RiverGeometry::new(stroke.points())),
        };
        Self {
            level_dm,
            shape,
            chunks: BTreeSet::new(),
            cells: 0,
            river,
        }
    }

    fn kind(&self) -> WaterKind {
        if self.river.is_some() {
            WaterKind::River
        } else {
            WaterKind::Lake
        }
    }

    pub(super) fn vertex_count(&self) -> usize {
        match &self.shape {
            BodyShape::Lake => 0,
            BodyShape::River { stroke, .. } => stroke.points().len(),
        }
    }
}

#[derive(Clone)]
pub(super) struct WaterState {
    pub(super) strict: bool,
    pub(super) revision: u64,
    pub(super) sea_level_dm: Option<i16>,
    pub(super) next_freshwater_id: u32,
    pub(super) bodies: BTreeMap<WaterBodyId, FreshBody>,
    pub(super) fresh: BTreeMap<TerrainChunkID, Box<FreshChunk>>,
    pub(super) cell_count: usize,
    pub(super) river_vertex_count: usize,
    sea: Vec<TileMask>,
    banks: BTreeMap<WaterBodyId, [Vec<BankEdge>; 2]>,
}

impl Default for WaterState {
    fn default() -> Self {
        Self {
            strict: false,
            revision: 0,
            sea_level_dm: None,
            next_freshwater_id: 1,
            bodies: BTreeMap::new(),
            fresh: BTreeMap::new(),
            cell_count: 0,
            river_vertex_count: 0,
            sea: Vec::new(),
            banks: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Occupant {
    body: WaterBodyId,
    kind: WaterKind,
    level_dm: i16,
    reach: Option<u16>,
}

impl Occupant {
    fn sea(level_dm: i16) -> Self {
        Self {
            body: WaterBodyId::SEA,
            kind: WaterKind::Sea,
            level_dm,
            reach: None,
        }
    }

    fn fresh(cell: FreshCell, body: &FreshBody) -> Self {
        let kind = body.kind();
        Self {
            body: WaterBodyId(cell.body),
            kind,
            level_dm: body.level_dm,
            reach: (kind == WaterKind::River).then_some(cell.reach),
        }
    }

    fn sample(self, bed: f32) -> WaterSample {
        let surface_z = f32::from(self.level_dm) / 10.0;
        WaterSample {
            body: self.body,
            kind: self.kind,
            surface_z,
            depth_m: (f64::from(self.level_dm) / 10.0 - f64::from(bed)) as f32,
            reach_m: self.reach,
        }
    }
}

impl WaterState {
    pub(super) fn occupies(&self, map: &Heightmap, point: Vec2) -> bool {
        if self.fresh.is_empty() && self.sea.is_empty() {
            return false;
        }
        let bounds = TileBounds::new(map);
        bounds
            .position(f64::from(point.x), f64::from(point.y))
            .is_some_and(|tile| self.occupant(bounds, tile).is_some())
    }

    fn fresh_cell(&self, tile: WaterTileCoord) -> FreshCell {
        self.fresh
            .get(&tile.chunk())
            .map_or(FreshCell::default(), |chunk| chunk[tile.local_index()])
    }

    fn occupant(&self, bounds: TileBounds, tile: WaterTileCoord) -> Option<Occupant> {
        if !bounds.contains(tile) {
            return None;
        }
        let cell = self.fresh_cell(tile);
        if cell.body != 0 {
            return Some(Occupant::fresh(cell, &self.bodies[&WaterBodyId(cell.body)]));
        }
        if sea_contains(&self.sea, bounds, tile) {
            return self.sea_level_dm.map(Occupant::sea);
        }
        None
    }
}

// Geometry predicates use f64 on exact f32 input coordinates. Tile contact has
// positive area iff a polygon edge enters the open tile, or its center is
// inside. The latter covers a tile fully contained by a simple polygon.
fn point(value: Vec2) -> Vec2d {
    Vec2d::new(f64::from(value.x), f64::from(value.y))
}

#[derive(Clone, Copy)]
struct Segment {
    a: Vec2d,
    b: Vec2d,
}

impl Segment {
    fn direction(self) -> Vec2d {
        self.b - self.a
    }

    fn projection(self, p: Vec2d) -> (f64, f64) {
        let d = self.direction();
        let t = ((p - self.a).dot(d) / d.dot(d)).clamp(0.0, 1.0);
        let closest = Vec2d::new(self.a.x + t * d.x, self.a.y + t * d.y);
        (p.distance2(closest), t)
    }

    fn side(self, p: Vec2d) -> f64 {
        self.direction().cross(p - self.a)
    }

    fn intersects(self, other: Self) -> bool {
        let a = self.side(other.a);
        let b = self.side(other.b);
        let c = other.side(self.a);
        let d = other.side(self.b);
        if ((a > 0.0 && b < 0.0) || (a < 0.0 && b > 0.0))
            && ((c > 0.0 && d < 0.0) || (c < 0.0 && d > 0.0))
        {
            return true;
        }
        (a == 0.0 && self.bounds_contain(other.a))
            || (b == 0.0 && self.bounds_contain(other.b))
            || (c == 0.0 && other.bounds_contain(self.a))
            || (d == 0.0 && other.bounds_contain(self.b))
    }

    fn bounds_contain(self, point: Vec2d) -> bool {
        point.x >= self.a.x.min(self.b.x)
            && point.x <= self.a.x.max(self.b.x)
            && point.y >= self.a.y.min(self.b.y)
            && point.y <= self.a.y.max(self.b.y)
    }

    fn distance2(self, other: Self) -> f64 {
        if self.intersects(other) {
            return 0.0;
        }
        self.projection(other.a)
            .0
            .min(self.projection(other.b).0)
            .min(other.projection(self.a).0)
            .min(other.projection(self.b).0)
    }

    fn enters(self, min: Vec2d, max: Vec2d, open: bool) -> bool {
        let d = self.direction();
        let mut lo: f64 = 0.0;
        let mut hi: f64 = 1.0;
        for (p, v, a, b) in [(self.a.x, d.x, min.x, max.x), (self.a.y, d.y, min.y, max.y)] {
            if v == 0.0 {
                if p < a || p > b || (open && (p == a || p == b)) {
                    return false;
                }
                continue;
            }
            let ta = (a - p) / v;
            let tb = (b - p) / v;
            lo = lo.max(ta.min(tb));
            hi = hi.min(ta.max(tb));
        }
        if open {
            lo < hi
        } else {
            lo <= hi
        }
    }

    fn distance2_to_tile(self, tile: WaterTileCoord) -> f64 {
        let min = point(tile.corner());
        let max = Vec2d::new(min.x + PITCH, min.y + PITCH);
        if self.enters(min, max, false) {
            return 0.0;
        }
        let rect_distance =
            |p: Vec2d| p.distance2(Vec2d::new(p.x.clamp(min.x, max.x), p.y.clamp(min.y, max.y)));
        [
            rect_distance(self.a),
            rect_distance(self.b),
            self.projection(min).0,
            self.projection(max).0,
            self.projection(Vec2d::new(min.x, max.y)).0,
            self.projection(Vec2d::new(max.x, min.y)).0,
        ]
        .into_iter()
        .fold(f64::INFINITY, f64::min)
    }

    /// A conservative tile span per row, including the radius around the segment.
    fn candidates(self, bounds: TileBounds, radius: f64) -> impl Iterator<Item = WaterTileCoord> {
        let (y0, y1) = span_axis(
            self.a.y.min(self.b.y) - radius,
            self.a.y.max(self.b.y) + radius,
            bounds.h,
        );
        (y0..y1).flat_map(move |y| {
            let min_y = f64::from(y) * PITCH - radius;
            let max_y = f64::from(y + 1) * PITCH + radius;
            let d = self.direction();
            let (a, b) = if d.y == 0.0 {
                (self.a.x, self.b.x)
            } else {
                let t0 = ((min_y - self.a.y) / d.y).clamp(0.0, 1.0);
                let t1 = ((max_y - self.a.y) / d.y).clamp(0.0, 1.0);
                (self.a.x + d.x * t0, self.a.x + d.x * t1)
            };
            let (x0, x1) = span_axis(a.min(b) - radius, a.max(b) + radius, bounds.w);
            (x0..x1).map(move |x| WaterTileCoord(x, y))
        })
    }
}

fn segments(points: &[Vec2], closed: bool) -> impl Iterator<Item = Segment> + '_ {
    let count = if closed {
        points.len()
    } else {
        points.len().saturating_sub(1)
    };
    (0..count).map(|i| Segment {
        a: point(points[i]),
        b: point(points[(i + 1) % points.len()]),
    })
}

fn validate_shape(points: &[Vec2], closed: bool) -> Result<(), WaterEditError> {
    if points.len() > MAX_WATER_VERTICES {
        return Err(WaterEditError::Limit);
    }
    if points.len() < if closed { 3 } else { 2 } || points.iter().any(|p| !p.is_finite()) {
        return Err(WaterEditError::InvalidInput);
    }
    let edges: Vec<_> = segments(points, closed).collect();
    for (i, &a) in edges.iter().enumerate() {
        if a.a.distance2(a.b) == 0.0 {
            return Err(WaterEditError::InvalidInput);
        }
        for (j, &b) in edges.iter().enumerate().skip(i + 1) {
            let adjacent = j == i + 1 || (closed && i == 0 && j + 1 == edges.len());
            if adjacent {
                let d = a.direction();
                let e = b.direction();
                if d.cross(e) == 0.0 && d.dot(e) < 0.0 {
                    return Err(WaterEditError::InvalidInput);
                }
            } else if a.intersects(b) {
                return Err(WaterEditError::InvalidInput);
            }
        }
    }
    Ok(())
}

fn river_length(points: &[Vec2]) -> Result<f64, WaterEditError> {
    let length = segments(points, false)
        .map(|s| s.a.distance2(s.b).sqrt())
        .sum::<f64>();
    if length > f64::from(u16::MAX) {
        return Err(WaterEditError::Limit);
    }
    Ok(length)
}

/// Prepared simple polygon for repeated positive-area tile overlap tests.
pub struct WaterFootprint {
    edges: Vec<Segment>,
    min: Vec2d,
    max: Vec2d,
}

impl WaterFootprint {
    pub fn new(points: &[Vec2]) -> Option<Self> {
        if points.len() < 3 || points.iter().any(|p| !p.is_finite()) {
            return None;
        }
        let first = point(points[0]);
        let mut min = first;
        let mut max = first;
        for &p in points {
            min.x = min.x.min(f64::from(p.x));
            min.y = min.y.min(f64::from(p.y));
            max.x = max.x.max(f64::from(p.x));
            max.y = max.y.max(f64::from(p.y));
        }
        Some(Self {
            edges: segments(points, true).collect(),
            min,
            max,
        })
    }

    pub fn overlaps_tile(&self, tile: WaterTileCoord) -> bool {
        let min = point(tile.corner());
        let max = Vec2d::new(min.x + PITCH, min.y + PITCH);
        let center = point(tile.center());
        let mut inside = false;
        for &edge in &self.edges {
            if edge.enters(min, max, true) && edge.a.distance2(edge.b) > 0.0 {
                return true;
            }
            if (edge.a.y > center.y) != (edge.b.y > center.y)
                && center.x
                    < edge.a.x
                        + (center.y - edge.a.y) * (edge.b.x - edge.a.x) / (edge.b.y - edge.a.y)
            {
                inside = !inside;
            }
        }
        inside
    }
}

/// Positive-area overlap against the closed geometric square. Boundary-only contact is dry.
/// The footprint must be a simple polygon (an OBB may be supplied as its four corners).
pub fn polygon_overlaps_water_tile(footprint: &Polygon, tile: WaterTileCoord) -> bool {
    WaterFootprint::new(&footprint.0).is_some_and(|shape| shape.overlaps_tile(tile))
}

#[derive(Clone)]
struct RiverGeometry {
    segments: Vec<Segment>,
    starts: Vec<f64>,
}

impl RiverGeometry {
    fn new(points: &[Vec2]) -> Self {
        let segments: Vec<_> = segments(points, false).collect();
        let mut starts = Vec::with_capacity(segments.len() + 1);
        starts.push(0.0);
        for s in &segments {
            starts.push(starts.last().unwrap() + s.a.distance2(s.b).sqrt());
        }
        Self { segments, starts }
    }

    fn reach(&self, segment: usize, fraction: f64) -> u16 {
        let start = self.starts[segment];
        (start + fraction * (self.starts[segment + 1] - start) + 0.5).floor() as u16
    }

    fn bank(&self, tile: WaterTileCoord, direction: usize, reach: u16) -> Option<RiverBank> {
        let center = point(tile.center());
        let reach_m = f64::from(reach);
        let first = self.starts[1..].partition_point(|&end| end < reach_m - 0.5);
        let end =
            self.starts[..self.segments.len()].partition_point(|&start| start < reach_m + 0.5);
        let mut best = None;
        for index in first..end {
            let (dist, t) = self.segments[index].projection(center);
            if self.reach(index, t) == reach && best.is_none_or(|(prev, _)| dist < prev) {
                best = Some((dist, index));
            }
        }
        let segment = self.segments[best?.1];
        let side = segment.side(point(tile.edge_point(direction)));
        if side > 0.0 {
            Some(RiverBank::Left)
        } else if side < 0.0 {
            Some(RiverBank::Right)
        } else {
            None
        }
    }

    fn rasterize(
        &self,
        bounds: TileBounds,
        width: f32,
        work: &mut Work,
    ) -> Result<Raster, WaterEditError> {
        if !valid_river_width(width) {
            return Err(WaterEditError::InvalidInput);
        }
        let radius = f64::from(width) * 0.5;
        for adjacent in self.segments.windows(2) {
            work.charge(1)?;
            let a = adjacent[0].direction();
            let b = adjacent[1].direction();
            let dot = a.dot(b);
            if dot < 0.0 && 2.0 * dot * dot >= a.dot(a) * b.dot(b) {
                return Err(WaterEditError::InvalidInput);
            }
        }
        for (i, &a) in self.segments.iter().enumerate() {
            for &b in self.segments.iter().skip(i + 2) {
                work.charge(1)?;
                // A folded pair of arms whose swept corridors overlap yields
                // conflicting local downstream directions. Nearby aligned
                // segments and ordinary bends may share their swept area.
                if a.direction().dot(b.direction()) <= 0.0
                    && a.distance2(b) < f64::from(width).powi(2)
                {
                    return Err(WaterEditError::InvalidInput);
                }
            }
        }

        struct Candidate {
            distance2: f64,
            segment: usize,
            reach: u16,
            covered: bool,
        }

        let mut candidates: BTreeMap<(TerrainChunkID, usize), Candidate> = BTreeMap::new();
        let mut tests = 0;
        for (index, &segment) in self.segments.iter().enumerate() {
            for tile in segment.candidates(bounds, radius) {
                tests += 1;
                if tests > MAX_WATER_TILE_TESTS {
                    return Err(WaterEditError::Limit);
                }
                work.charge(1)?;
                let hit = segment.distance2_to_tile(tile) < radius * radius;
                let (distance2, fraction) = segment.projection(point(tile.center()));
                let candidate = candidates
                    .entry((tile.chunk(), tile.local_index()))
                    .or_insert(Candidate {
                        distance2,
                        segment: index,
                        reach: self.reach(index, fraction),
                        covered: false,
                    });
                if distance2 < candidate.distance2 {
                    candidate.distance2 = distance2;
                    candidate.segment = index;
                    candidate.reach = self.reach(index, fraction);
                }
                candidate.covered |= hit;
                if candidates.len() > MAX_FRESHWATER_CANDIDATES {
                    return Err(WaterEditError::Limit);
                }
            }
        }

        // A segment can be nearest to a covered tile's center without
        // intersecting that tile's swept corridor. Search the extra
        // half-diagonal against the existing candidate raster only.
        let nearest_radius = radius + PITCH * std::f64::consts::FRAC_1_SQRT_2;
        for (index, &segment) in self.segments.iter().enumerate() {
            for tile in segment.candidates(bounds, nearest_radius) {
                tests += 1;
                if tests > MAX_WATER_TILE_TESTS {
                    return Err(WaterEditError::Limit);
                }
                work.charge(1)?;
                if let Some(candidate) = candidates
                    .get_mut(&(tile.chunk(), tile.local_index()))
                    .filter(|c| c.covered)
                {
                    let (distance2, fraction) = segment.projection(point(tile.center()));
                    let reach = self.reach(index, fraction);
                    if (distance2, index) < (candidate.distance2, candidate.segment) {
                        candidate.distance2 = distance2;
                        candidate.segment = index;
                        candidate.reach = reach;
                    }
                }
            }
        }

        Ok(candidates
            .into_iter()
            .filter_map(|(key, value)| value.covered.then_some((key, value.reach)))
            .collect())
    }
}

type Raster = BTreeMap<(TerrainChunkID, usize), u16>;

fn rasterize_lake(
    bounds: TileBounds,
    basin: &WaterBasin,
    work: &mut Work,
) -> Result<Raster, WaterEditError> {
    let shape = WaterFootprint::new(basin.points()).ok_or(WaterEditError::InvalidInput)?;
    let (x0, x1) = span_axis(shape.min.x, shape.max.x, bounds.w);
    let (y0, y1) = span_axis(shape.min.y, shape.max.y, bounds.h);
    let count = (x1 - x0) as usize * (y1 - y0) as usize;
    let tests = count
        .checked_mul(shape.edges.len())
        .ok_or(WaterEditError::Limit)?;
    if count > MAX_FRESHWATER_CANDIDATES || tests > MAX_WATER_TILE_TESTS {
        return Err(WaterEditError::Limit);
    }
    work.charge(tests)?;
    Ok(bounds
        .span(shape.min, shape.max)
        .filter(|&tile| shape.overlaps_tile(tile))
        .map(|tile| ((tile.chunk(), tile.local_index()), 0))
        .collect())
}

fn validate_connected(mut cells: TileSet, count: usize) -> Result<(), WaterEditError> {
    let Some(first) = cells.tiles().next() else {
        return Err(WaterEditError::InvalidInput);
    };
    let mut todo = vec![first];
    cells.0.get_mut(&first.chunk()).unwrap().0[first.local_index() / 64] &=
        !(1 << (first.local_index() % 64));
    let mut visited = 0;
    while let Some(tile) = todo.pop() {
        visited += 1;
        for direction in 0..DIRECTIONS.len() {
            let next = tile.neighbor(direction);
            if let Some(mask) = cells.0.get_mut(&next.chunk()) {
                let index = next.local_index();
                if mask.contains(index) {
                    mask.0[index / 64] &= !(1 << (index % 64));
                    todo.push(next);
                }
            }
        }
    }
    if visited != count {
        return Err(WaterEditError::Disconnected(cells.tiles().next().unwrap()));
    }
    Ok(())
}

struct Work(usize);

impl Work {
    fn charge(&mut self, amount: usize) -> Result<(), WaterEditError> {
        self.0 = self
            .0
            .checked_add(amount)
            .filter(|&work| work <= MAX_WATER_BATCH_WORK)
            .ok_or(WaterEditError::Limit)?;
        Ok(())
    }
}

pub(super) fn validate_map_dimensions(map: &Heightmap) -> Result<(), WaterEditError> {
    if map.w > MAX_WATER_CHUNKS_PER_AXIS || map.h > MAX_WATER_CHUNKS_PER_AXIS {
        return Err(WaterEditError::Limit);
    }
    Ok(())
}

pub(super) fn valid_river_width(width_m: f32) -> bool {
    (32.0..=512.0).contains(&width_m)
}

pub(super) fn valid_water_depth(depth_m: f64) -> bool {
    depth_m.is_finite() && depth_m >= MIN_DEPTH
}

pub(super) fn packed_bed_limit(level_dm: i16, depth_m: f64) -> Result<u16, WaterEditError> {
    let level = f64::from(level_dm) / 10.0;
    if !valid_water_depth(depth_m) {
        return Err(WaterEditError::InvalidInput);
    }
    pack_base_height_at_most(level - depth_m).ok_or(WaterEditError::UnrepresentableLevel)
}

fn tile_node(tile: WaterTileCoord) -> BaseNode {
    (tile.0 as usize, tile.1 as usize)
}

fn incident_tiles(bounds: TileBounds, (x, y): BaseNode) -> impl Iterator<Item = WaterTileCoord> {
    let (x, y) = (x as i16, y as i16);
    [(x - 1, y - 1), (x - 1, y), (x, y - 1), (x, y)]
        .into_iter()
        .map(|(x, y)| WaterTileCoord(x, y))
        .filter(move |&tile| bounds.contains(tile))
}

fn sea_contains(masks: &[TileMask], bounds: TileBounds, tile: WaterTileCoord) -> bool {
    bounds.contains(tile)
        && masks
            .get(bounds.index(tile.chunk()))
            .is_some_and(|mask| mask.contains(tile.local_index()))
}

struct Bed<'a> {
    map: &'a Heightmap,
    patch: Option<&'a BaseNodePatch>,
}

impl Bed<'_> {
    fn packed(&self, node: BaseNode) -> u16 {
        match self.patch {
            Some(patch) => patch.packed_base_node(self.map, node).unwrap(),
            None => self.map.packed_base_node(node).unwrap(),
        }
    }

    fn submerged(&self, tile: WaterTileCoord, limit: u16) -> bool {
        self.map
            .base_tile_nodes(tile_node(tile))
            .unwrap()
            .into_iter()
            .all(|node| self.packed(node) <= limit)
    }
}

fn rebuild_sea(
    bounds: TileBounds,
    bed: Bed<'_>,
    fresh: impl Fn(WaterTileCoord) -> FreshCell,
    level_dm: Option<i16>,
) -> Result<Vec<TileMask>, WaterEditError> {
    let Some(level_dm) = level_dm else {
        return Ok(Vec::new());
    };
    let limit = packed_bed_limit(level_dm, MIN_DEPTH)?;
    if bounds.w == 0 || bounds.h == 0 {
        return Ok(Vec::new());
    }

    struct Frontier {
        seen: Vec<TileMask>,
        wet: Vec<TileMask>,
        pending: Vec<TileMask>,
        queued: Vec<bool>,
        chunks: VecDeque<usize>,
        bounds: TileBounds,
    }

    impl Frontier {
        fn visit(&mut self, tile: WaterTileCoord, eligible: &impl Fn(WaterTileCoord) -> bool) {
            if !self.bounds.contains(tile) {
                return;
            }
            let chunk = self.bounds.index(tile.chunk());
            let index = tile.local_index();
            if self.seen[chunk].contains(index) {
                return;
            }
            self.seen[chunk].insert(index);
            if eligible(tile) {
                self.wet[chunk].insert(index);
                self.pending[chunk].insert(index);
                if !self.queued[chunk] {
                    self.chunks.push_back(chunk);
                    self.queued[chunk] = true;
                }
            }
        }
    }

    let n = bounds.chunks();
    let mut frontier = Frontier {
        seen: vec![TileMask::default(); n],
        wet: vec![TileMask::default(); n],
        pending: vec![TileMask::default(); n],
        queued: vec![false; n],
        chunks: VecDeque::new(),
        bounds,
    };
    let eligible = |tile| fresh(tile).body == 0 && bed.submerged(tile, limit);
    for x in 0..bounds.w {
        frontier.visit(WaterTileCoord(x, 0), &eligible);
        frontier.visit(WaterTileCoord(x, bounds.h - 1), &eligible);
    }
    for y in 0..bounds.h {
        frontier.visit(WaterTileCoord(0, y), &eligible);
        frontier.visit(WaterTileCoord(bounds.w - 1, y), &eligible);
    }

    while let Some(index) = frontier.chunks.pop_front() {
        let chunk = bounds.chunk(index);
        loop {
            let pending = std::mem::take(&mut frontier.pending[index]);
            if pending.is_empty() {
                break;
            }
            for local in pending.indices() {
                let tile = WaterTileCoord::from_local(chunk, local);
                for direction in 0..DIRECTIONS.len() {
                    frontier.visit(tile.neighbor(direction), &eligible);
                }
            }
        }
        frontier.queued[index] = false;
    }
    if frontier.wet.iter().all(TileMask::is_empty) {
        return Ok(Vec::new());
    }
    Ok(frontier.wet)
}

fn validate_mouth(
    tile: WaterTileCoord,
    occupant: impl Fn(WaterTileCoord) -> Option<Occupant>,
) -> Result<(), WaterEditError> {
    let Some(water) = occupant(tile) else {
        return Ok(());
    };
    for direction in 0..DIRECTIONS.len() {
        let next = tile.neighbor(direction);
        if occupant(next).is_some_and(|other| other.level_dm != water.level_dm) {
            return Err(WaterEditError::IncompatibleMouth(tile, next));
        }
    }
    Ok(())
}

#[derive(Default)]
struct FreshDelta {
    body: Option<BodyDelta>,
    chunks: BTreeMap<TerrainChunkID, Option<Box<FreshChunk>>>,
}

enum BodyDelta {
    Insert(WaterBodyId, FreshBody),
    Remove(WaterBodyId),
}

impl FreshDelta {
    fn cell(&self, old: &WaterState, tile: WaterTileCoord) -> FreshCell {
        match self.chunks.get(&tile.chunk()) {
            Some(Some(chunk)) => chunk[tile.local_index()],
            Some(None) => FreshCell::default(),
            None => old.fresh_cell(tile),
        }
    }

    fn body<'a>(&'a self, old: &'a WaterState, id: WaterBodyId) -> Option<&'a FreshBody> {
        match &self.body {
            Some(BodyDelta::Insert(created, body)) if id == *created => Some(body),
            Some(BodyDelta::Remove(removed)) if id == *removed => None,
            _ => old.bodies.get(&id),
        }
    }

    fn insert(
        state: &WaterState,
        level_dm: i16,
        shape: BodyShape,
        raster: Raster,
        coverage: &mut TileSet,
    ) -> Result<Self, WaterEditError> {
        let id = WaterBodyId(
            u16::try_from(state.next_freshwater_id).map_err(|_| WaterEditError::BodyIdExhausted)?,
        );
        let mut body = FreshBody::new(level_dm, shape);
        body.cells = raster.len();
        if state.river_vertex_count + body.vertex_count() > MAX_WATER_RIVER_VERTICES
            || state.cell_count + body.cells > MAX_FRESHWATER_CELLS
        {
            return Err(WaterEditError::Limit);
        }
        let mut chunks: BTreeMap<_, Option<Box<FreshChunk>>> = BTreeMap::new();
        for ((chunk, local), reach) in raster {
            let target = chunks.entry(chunk).or_insert_with(|| {
                Some(
                    state
                        .fresh
                        .get(&chunk)
                        .cloned()
                        .unwrap_or_else(empty_fresh_chunk),
                )
            });
            let slot = &mut target.as_mut().unwrap()[local];
            let tile = WaterTileCoord::from_local(chunk, local);
            if slot.body != 0 {
                return Err(WaterEditError::Overlap(tile));
            }
            *slot = FreshCell { body: id.0, reach };
            coverage.insert(tile);
            body.chunks.insert(chunk);
        }
        validate_connected(TileSet(coverage.0.clone()), body.cells)?;
        Ok(Self {
            body: Some(BodyDelta::Insert(id, body)),
            chunks,
        })
    }

    fn remove(
        state: &WaterState,
        id: WaterBodyId,
        coverage: &mut TileSet,
    ) -> Result<Self, WaterEditError> {
        let body = state
            .bodies
            .get(&id)
            .ok_or(WaterEditError::BodyUnavailable(id))?;
        let mut chunks = BTreeMap::new();
        for &chunk in &body.chunks {
            let mut replacement = state.fresh[&chunk].clone();
            for (local, cell) in replacement.iter_mut().enumerate() {
                if cell.body == id.0 {
                    *cell = FreshCell::default();
                    coverage.insert(WaterTileCoord::from_local(chunk, local));
                }
            }
            let has_water = replacement.iter().any(|cell| cell.body != 0);
            chunks.insert(chunk, has_water.then_some(replacement));
        }
        Ok(Self {
            body: Some(BodyDelta::Remove(id)),
            chunks,
        })
    }

    fn apply(self, state: &mut WaterState) {
        match self.body {
            Some(BodyDelta::Insert(id, body)) => {
                state.cell_count += body.cells;
                state.river_vertex_count += body.vertex_count();
                state.bodies.insert(id, body);
                state.next_freshwater_id = u32::from(id.0) + 1;
            }
            Some(BodyDelta::Remove(id)) => {
                let body = state.bodies.remove(&id).unwrap();
                state.cell_count -= body.cells;
                state.river_vertex_count -= body.vertex_count();
                state.banks.remove(&id);
            }
            None => {}
        }
        for (chunk, replacement) in self.chunks {
            if let Some(cells) = replacement {
                state.fresh.insert(chunk, cells);
            } else {
                state.fresh.remove(&chunk);
            }
        }
    }
}

pub(super) fn empty_fresh_chunk() -> Box<FreshChunk> {
    Box::new([FreshCell::default(); TILES_PER_CHUNK])
}

impl TileSet {
    fn dependency_chunks(&self, bounds: TileBounds) -> BTreeSet<TerrainChunkID> {
        let mut chunks = BTreeSet::new();
        const FIRST_COLUMN: u64 = 0x0000_0001_0000_0001;
        const LAST_COLUMN: u64 = 0x8000_0000_8000_0000;
        for (&chunk, mask) in &self.0 {
            chunks.insert(chunk);
            let left = mask.0.iter().any(|word| (word & FIRST_COLUMN) != 0);
            let right = mask.0.iter().any(|word| (word & LAST_COLUMN) != 0);
            let bottom = mask.0[0] as u32 != 0;
            let top = mask.0[mask.0.len() - 1] >> 32 != 0;
            for (dx, dy, hit) in [
                (-1, 0, left),
                (1, 0, right),
                (0, -1, bottom),
                (0, 1, top),
                (-1, -1, mask.contains(0)),
                (1, -1, mask.contains(RES as usize - 1)),
                (-1, 1, mask.contains(TILES_PER_CHUNK - RES as usize)),
                (1, 1, mask.contains(TILES_PER_CHUNK - 1)),
            ] {
                let neighbor = TerrainChunkID::new_i16(chunk.0 + dx, chunk.1 + dy);
                if hit && bounds.contains_chunk(neighbor) {
                    chunks.insert(neighbor);
                }
            }
        }
        chunks
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BankEdge {
    reach: u16,
    edge: u32,
}

impl BankEdge {
    fn new(tile: WaterTileCoord, direction: usize, reach: u16) -> Self {
        // Order by the actual edge's lower endpoint. Opposite directions
        // share the same geometric edge and cannot both be wet-to-dry.
        let x = tile.0 + i16::from(direction == 3);
        let y = tile.1 + i16::from(direction == 2);
        let edge = ((x as u32) << 14) | ((y as u32) << 2) | direction as u32;
        Self { reach, edge }
    }

    fn tile(self) -> WaterTileCoord {
        let x = (self.edge >> 14) as i16 - i16::from(self.direction() == 3);
        let y = ((self.edge >> 2) & 0xfff) as i16 - i16::from(self.direction() == 2);
        WaterTileCoord(x, y)
    }

    fn direction(self) -> usize {
        (self.edge & 3) as usize
    }
}

type BankChanges = BTreeMap<WaterBodyId, [Vec<(BankEdge, bool)>; 2]>;

/// An immutable candidate until the owning map has checked its structures and occupancy.
pub struct PreparedWaterEdit {
    revision: u64,
    base_revision: u64,
    bounds: TileBounds,
    fresh: FreshDelta,
    sea_level_dm: Option<i16>,
    sea: Option<Vec<TileMask>>,
    patch: BaseNodePatch,
    affected: TileSet,
    surface_changes: TileSet,
    strict: bool,
    next_revision: u64,
    body: Option<WaterBodyId>,
    level_dm: Option<i16>,
    dirty: WaterDirtyChunks,
    work: usize,
}

impl PreparedWaterEdit {
    fn occupant(&self, env: &Environment, tile: WaterTileCoord) -> Option<Occupant> {
        if !self.bounds.contains(tile) {
            return None;
        }
        let cell = self.fresh.cell(&env.water, tile);
        if cell.body != 0 {
            let body = self.fresh.body(&env.water, WaterBodyId(cell.body)).unwrap();
            return Some(Occupant::fresh(cell, body));
        }
        let sea = self.sea.as_deref().unwrap_or(&env.water.sea);
        if sea_contains(sea, self.bounds, tile) {
            return self.sea_level_dm.map(Occupant::sea);
        }
        None
    }

    pub fn affected_tiles(&self) -> impl Iterator<Item = WaterTileCoord> + '_ {
        self.affected.tiles()
    }

    pub fn surface_changed_tiles(&self) -> impl Iterator<Item = WaterTileCoord> + '_ {
        self.surface_changes.tiles()
    }

    pub fn water_at(&self, env: &Environment, point: Vec2) -> Option<WaterSample> {
        let tile = self
            .bounds
            .position(f64::from(point.x), f64::from(point.y))?;
        Some(
            self.occupant(env, tile)?
                .sample(self.base_height(env, point)?),
        )
    }

    pub fn water_at_tile(&self, env: &Environment, tile: WaterTileCoord) -> Option<WaterTile> {
        let sample = self.water_at(env, tile.center())?;
        Some(WaterTile { tile, sample })
    }

    pub fn is_wet_base_node(&self, env: &Environment, node: BaseNode) -> bool {
        env.heightmap.packed_base_node(node).is_some()
            && incident_tiles(self.bounds, node).any(|tile| self.occupant(env, tile).is_some())
    }

    pub fn changed(&self) -> bool {
        self.next_revision != self.revision
    }

    pub fn body(&self) -> Option<WaterBodyId> {
        self.body
    }

    pub fn base_height(&self, env: &Environment, point: Vec2) -> Option<f32> {
        self.patch.base_height(&env.heightmap, point)
    }

    pub fn intersects_water(&self, env: &Environment, footprint: &Polygon) -> bool {
        intersects_tiles(self.bounds, footprint, |tile| {
            self.occupant(env, tile).is_some()
        })
    }

    pub fn dirty(&self) -> &WaterDirtyChunks {
        &self.dirty
    }

    pub fn strict_water_siting(&self) -> bool {
        self.strict
    }

    pub fn proposed_surface_dm(&self) -> Option<i16> {
        self.level_dm
    }

    pub fn work(&self) -> usize {
        self.work
    }

    fn changed_near_freshwater<'a>(
        &'a self,
        env: &'a Environment,
    ) -> impl Iterator<Item = WaterTileCoord> + 'a {
        self.surface_changes
            .0
            .iter()
            .filter(move |&(chunk, _)| {
                let has_fresh =
                    |id| env.water.fresh.contains_key(&id) || self.fresh.chunks.contains_key(&id);
                has_fresh(*chunk)
                    || DIRECTIONS.iter().any(|&(dx, dy)| {
                        has_fresh(TerrainChunkID::new_i16(chunk.0 + dx, chunk.1 + dy))
                    })
            })
            .flat_map(|(&chunk, &mask)| {
                mask.indices()
                    .map(move |index| WaterTileCoord::from_local(chunk, index))
            })
    }

    fn bank_changes(&self, env: &Environment) -> BankChanges {
        let mut changes: BankChanges = BTreeMap::new();
        let old = |tile| env.water.occupant(self.bounds, tile);
        let new = |tile| self.occupant(env, tile);
        for tile in self.changed_near_freshwater(env) {
            let old_tile = old(tile);
            let new_tile = new(tile);
            for direction in 0..DIRECTIONS.len() {
                let neighbor = tile.neighbor(direction);
                if !self.bounds.contains(neighbor) {
                    continue;
                }
                let old_neighbor = old(neighbor);
                let new_neighbor = new(neighbor);
                for (wet_tile, edge, before, after, dry_before, dry_after) in [
                    (
                        tile,
                        direction,
                        old_tile,
                        new_tile,
                        old_neighbor.is_none(),
                        new_neighbor.is_none(),
                    ),
                    (
                        neighbor,
                        3 - direction,
                        old_neighbor,
                        new_neighbor,
                        old_tile.is_none(),
                        new_tile.is_none(),
                    ),
                ] {
                    let before =
                        before.filter(|water| dry_before && water.kind == WaterKind::River);
                    let after = after.filter(|water| dry_after && water.kind == WaterKind::River);
                    if before == after {
                        continue;
                    }
                    for (water, insert) in [(before, false), (after, true)] {
                        let Some(water) = water else { continue };
                        let Some(river) = self
                            .fresh
                            .body(&env.water, water.body)
                            .and_then(|record| record.river.as_ref())
                        else {
                            continue;
                        };
                        let reach = water.reach.unwrap();
                        if let Some(bank) = river.bank(wet_tile, edge, reach) {
                            changes.entry(water.body).or_default()[bank.index()]
                                .push((BankEdge::new(wet_tile, edge, reach), insert));
                        }
                    }
                }
            }
        }
        changes
    }
}

fn merge_banks(state: &mut WaterState, changes: BankChanges) {
    for (body, banks) in changes {
        let retained = state.banks.entry(body).or_default();
        for (edges, mut changes) in retained.iter_mut().zip(banks) {
            if changes.is_empty() {
                continue;
            }
            changes.sort_unstable();
            changes.dedup();
            let mut old = std::mem::take(edges).into_iter().peekable();
            for (edge, insert) in changes {
                while old.peek().is_some_and(|&candidate| candidate < edge) {
                    edges.push(old.next().unwrap());
                }
                old.next_if_eq(&edge);
                if insert {
                    edges.push(edge);
                }
            }
            edges.extend(old);
        }
    }
}

fn packed_carve(
    map: &Heightmap,
    patch: &mut BaseNodePatch,
    coverage: &TileSet,
    level_dm: i16,
    depth_m: f32,
) -> Result<(), WaterEditError> {
    let limit = packed_bed_limit(level_dm, f64::from(depth_m))?;
    for tile in coverage.tiles() {
        for node in map.base_tile_nodes(tile_node(tile)).unwrap() {
            let old = patch.packed_base_node(map, node).unwrap();
            if old > limit {
                patch.stage(map, node, limit)?;
            }
        }
    }
    Ok(())
}

impl Environment {
    pub fn sea_level_dm(&self) -> Option<i16> {
        self.water.sea_level_dm
    }

    pub fn strict_water_siting(&self) -> bool {
        self.water.strict
    }

    pub fn has_water_configuration(&self) -> bool {
        self.water.sea_level_dm.is_some() || !self.water.bodies.is_empty()
    }

    pub fn water_revision(&self) -> u64 {
        self.water.revision
    }

    pub fn freshwater_bodies(&self) -> impl Iterator<Item = (WaterBodyId, WaterKind, i16)> + '_ {
        self.water
            .bodies
            .iter()
            .map(|(&id, body)| (id, body.kind(), body.level_dm))
    }

    pub fn river_centerline(&self, id: WaterBodyId) -> Option<&[Vec2]> {
        match &self.water.bodies.get(&id)?.shape {
            BodyShape::River { stroke, .. } => Some(stroke.points()),
            BodyShape::Lake => None,
        }
    }

    pub fn base_height(&self, point: Vec2) -> Option<f32> {
        self.heightmap.base_height(point)
    }

    /// Authoring entry point for an unpublished terrain generator. Live maps use the map transaction.
    pub fn set_sea_level(
        &mut self,
        level_dm: Option<i16>,
    ) -> Result<WaterEditOutcome, WaterEditError> {
        let prepared = self.prepare_water_edit(&WaterEdit::SetSeaLevel { level_dm })?;
        self.commit_prepared_water(prepared)
    }

    /// Authoring entry point for an unpublished terrain generator.
    pub fn carve_river(
        &mut self,
        polyline: &[Vec2],
        width_m: f32,
        depth_m: f32,
    ) -> Result<WaterBodyId, WaterEditError> {
        self.carve_river_at_level(polyline, width_m, depth_m, None)
    }

    /// Authoring entry point for an unpublished terrain generator.
    pub fn carve_river_at_level(
        &mut self,
        polyline: &[Vec2],
        width_m: f32,
        depth_m: f32,
        level_dm: Option<i16>,
    ) -> Result<WaterBodyId, WaterEditError> {
        let polyline = WaterStroke::new(polyline)?;
        let prepared = self.prepare_water_edit(&WaterEdit::CarveRiver {
            polyline,
            width_m,
            depth_m,
            level_dm,
        })?;
        Ok(self.commit_prepared_water(prepared)?.body.unwrap())
    }

    /// Authoring entry point for an unpublished terrain generator.
    pub fn fill_lake(
        &mut self,
        basin: &Polygon,
        level_dm: i16,
        depth_m: f32,
    ) -> Result<WaterBodyId, WaterEditError> {
        let basin = WaterBasin::new(&basin.0)?;
        let prepared = self.prepare_water_edit(&WaterEdit::FillLake {
            basin,
            level_dm,
            depth_m,
        })?;
        Ok(self.commit_prepared_water(prepared)?.body.unwrap())
    }

    /// Authoring entry point for an unpublished terrain generator.
    pub fn remove_freshwater(
        &mut self,
        body: WaterBodyId,
    ) -> Result<WaterEditOutcome, WaterEditError> {
        let prepared = self.prepare_water_edit(&WaterEdit::RemoveFreshWater { body })?;
        self.commit_prepared_water(prepared)
    }

    pub(crate) fn prepare_water_edit(
        &self,
        edit: &WaterEdit,
    ) -> Result<PreparedWaterEdit, WaterEditError> {
        validate_map_dimensions(&self.heightmap)?;
        let bounds = TileBounds::new(&self.heightmap);
        let mut work = Work(0);
        let mut fresh = FreshDelta::default();
        let mut coverage = TileSet::default();
        let mut patch = BaseNodePatch::new(MAX_WATER_BASE_WRITES);
        let mut sea_level_dm = self.water.sea_level_dm;
        let (body_id, level) = match edit {
            WaterEdit::SetSeaLevel { level_dm } => {
                if let Some(level) = level_dm {
                    packed_bed_limit(*level, MIN_DEPTH)?;
                }
                sea_level_dm = *level_dm;
                (level_dm.map(|_| WaterBodyId::SEA), *level_dm)
            }
            WaterEdit::CarveRiver {
                polyline,
                width_m,
                depth_m,
                level_dm,
            } => {
                bounds.validate_vertices(polyline.points())?;
                let n = polyline.points().len();
                work.charge(n * (n - 1) / 2)?;
                let level = match level_dm {
                    Some(level) => *level,
                    None => {
                        let downstream = *polyline.points().last().unwrap();
                        let datum = self
                            .base_height(downstream)
                            .ok_or(WaterEditError::OutOfBounds)?;
                        ((f64::from(datum) - 1.0) * 10.0 + 0.5).floor() as i16
                    }
                };
                packed_bed_limit(level, f64::from(*depth_m))?;
                let river = RiverGeometry::new(polyline.points());
                let raster = river.rasterize(bounds, *width_m, &mut work)?;
                work.charge(raster.len() * 12)?;
                let shape = BodyShape::River {
                    stroke: polyline.clone(),
                    width_m: *width_m,
                };
                fresh = FreshDelta::insert(&self.water, level, shape, raster, &mut coverage)?;
                packed_carve(&self.heightmap, &mut patch, &coverage, level, *depth_m)?;
                (
                    Some(WaterBodyId(self.water.next_freshwater_id as u16)),
                    Some(level),
                )
            }
            WaterEdit::FillLake {
                basin,
                level_dm,
                depth_m,
            } => {
                bounds.validate_vertices(basin.points())?;
                let n = basin.points().len();
                work.charge(n * (n - 1) / 2)?;
                packed_bed_limit(*level_dm, f64::from(*depth_m))?;
                let raster = rasterize_lake(bounds, basin, &mut work)?;
                work.charge(raster.len() * 12)?;
                fresh = FreshDelta::insert(
                    &self.water,
                    *level_dm,
                    BodyShape::Lake,
                    raster,
                    &mut coverage,
                )?;
                packed_carve(&self.heightmap, &mut patch, &coverage, *level_dm, *depth_m)?;
                (
                    Some(WaterBodyId(self.water.next_freshwater_id as u16)),
                    Some(*level_dm),
                )
            }
            WaterEdit::RemoveFreshWater { body } => {
                let info = self
                    .water
                    .bodies
                    .get(body)
                    .ok_or(WaterEditError::BodyUnavailable(*body))?;
                work.charge(info.chunks.len() * TILES_PER_CHUNK * 2)?;
                fresh = FreshDelta::remove(&self.water, *body, &mut coverage)?;
                (Some(*body), None)
            }
        };
        self.prepare_hydrology(patch, fresh, coverage, sea_level_dm, body_id, level, work)
    }

    /// Prepare a terrain-only patch through the same bed and sea checks as a water command.
    pub(crate) fn prepare_water_base_patch(
        &self,
        patch: BaseNodePatch,
    ) -> Result<PreparedWaterEdit, WaterEditError> {
        self.prepare_water_base_patch_with_work(patch, 0)
    }

    pub(super) fn prepare_water_base_patch_with_work(
        &self,
        patch: BaseNodePatch,
        work: usize,
    ) -> Result<PreparedWaterEdit, WaterEditError> {
        validate_map_dimensions(&self.heightmap)?;
        self.prepare_hydrology(
            patch,
            FreshDelta::default(),
            TileSet::default(),
            self.water.sea_level_dm,
            None,
            None,
            Work(work),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_hydrology(
        &self,
        patch: BaseNodePatch,
        fresh: FreshDelta,
        mut surface_changes: TileSet,
        sea_level_dm: Option<i16>,
        body: Option<WaterBodyId>,
        level_dm: Option<i16>,
        mut work: Work,
    ) -> Result<PreparedWaterEdit, WaterEditError> {
        let map = &self.heightmap;
        let bounds = TileBounds::new(map);
        let mut beds = TileSet::default();
        if patch.staged_node_count() > MAX_WATER_BASE_WRITES {
            return Err(WaterEditError::Limit);
        }
        let mut writes = 0;
        for (node, old, _) in patch.changes() {
            match map.packed_base_node(node) {
                None => return Err(WaterEditError::OutOfBounds),
                Some(current) if current != old => return Err(WaterEditError::Stale),
                Some(_) => {}
            }
            writes += 1;
            work.charge(8)?;
            for tile in incident_tiles(bounds, node) {
                beds.insert(tile);
            }
        }
        let old_bed = Bed { map, patch: None };
        let new_bed = Bed {
            map,
            patch: (writes != 0).then_some(&patch),
        };
        let mut local = TileSet(beds.0.clone());
        for (&chunk, &mask) in &surface_changes.0 {
            local.union_mask(chunk, mask);
        }
        for tile in local.tiles() {
            let cell = fresh.cell(&self.water, tile);
            if cell.body != 0 {
                let info = fresh.body(&self.water, WaterBodyId(cell.body)).unwrap();
                if !new_bed.submerged(tile, packed_bed_limit(info.level_dm, MIN_DEPTH)?) {
                    return Err(WaterEditError::InvalidBed(tile));
                }
            }
        }

        let level_changed = sea_level_dm != self.water.sea_level_dm;
        let mut rebuild = level_changed;
        if !rebuild {
            if let Some(level) = sea_level_dm {
                let limit = packed_bed_limit(level, MIN_DEPTH)?;
                let was_sea = |tile| sea_contains(&self.water.sea, bounds, tile);
                rebuild = local.tiles().any(|tile| {
                    let before =
                        self.water.fresh_cell(tile).body == 0 && old_bed.submerged(tile, limit);
                    let after =
                        fresh.cell(&self.water, tile).body == 0 && new_bed.submerged(tile, limit);
                    if before == after {
                        return false;
                    }
                    if before {
                        return was_sea(tile);
                    }
                    DIRECTIONS.iter().enumerate().any(|(dir, _)| {
                        let neighbor = tile.neighbor(dir);
                        !bounds.contains(neighbor) || was_sea(neighbor)
                    })
                });
            }
        }
        let sea = if rebuild {
            Some(rebuild_sea(
                bounds,
                new_bed,
                |tile| fresh.cell(&self.water, tile),
                sea_level_dm,
            )?)
        } else {
            None
        };
        if let Some(sea) = &sea {
            for index in 0..sea.len().max(self.water.sea.len()) {
                let old = self.water.sea.get(index).copied().unwrap_or_default();
                let new = sea.get(index).copied().unwrap_or_default();
                surface_changes.union_mask(bounds.chunk(index), old.diff(new, !level_changed));
            }
        }
        let changed = writes != 0 || level_changed || fresh.body.is_some();
        let next_revision = if changed {
            self.water
                .revision
                .checked_add(1)
                .ok_or(WaterEditError::Limit)?
        } else {
            self.water.revision
        };
        let strict = self.water.strict || !surface_changes.0.is_empty();
        let mut affected = TileSet(beds.0.clone());
        for (&chunk, &mask) in &surface_changes.0 {
            affected.union_mask(chunk, mask);
        }
        let dirty = WaterDirtyChunks {
            water: surface_changes.dependency_chunks(bounds),
            terrain: beds.dependency_chunks(bounds),
            road_support: affected.dependency_chunks(bounds),
        };
        let candidate = PreparedWaterEdit {
            revision: self.water.revision,
            base_revision: self.base_revision,
            bounds,
            fresh,
            sea_level_dm,
            sea,
            patch,
            affected,
            surface_changes,
            strict,
            next_revision,
            body,
            level_dm,
            dirty,
            work: work.0,
        };
        for tile in candidate.changed_near_freshwater(self) {
            validate_mouth(tile, |tile| candidate.occupant(self, tile))?;
        }
        Ok(candidate)
    }

    pub(crate) fn commit_prepared_water(
        &mut self,
        prepared: PreparedWaterEdit,
    ) -> Result<WaterEditOutcome, WaterEditError> {
        if self.water.revision != prepared.revision
            || self.base_revision != prepared.base_revision
            || TileBounds::new(&self.heightmap) != prepared.bounds
        {
            return Err(WaterEditError::Stale);
        }
        let changed = prepared.next_revision != prepared.revision;
        if changed {
            let changes = prepared.bank_changes(self);
            self.heightmap.apply_base_patch(prepared.patch)?;
            if let Some(sea) = prepared.sea {
                self.water.sea = sea;
            }
            self.water.sea_level_dm = prepared.sea_level_dm;
            self.water.strict = prepared.strict;
            self.water.revision = prepared.next_revision;
            prepared.fresh.apply(&mut self.water);
            merge_banks(&mut self.water, changes);
        }
        Ok(WaterEditOutcome {
            changed,
            body: prepared.body,
            dirty: prepared.dirty,
        })
    }
}

fn intersects_tiles(
    bounds: TileBounds,
    footprint: &Polygon,
    occupied: impl Fn(WaterTileCoord) -> bool,
) -> bool {
    let Some(shape) = WaterFootprint::new(&footprint.0) else {
        return false;
    };
    bounds
        .span(shape.min, shape.max)
        .any(|tile| occupied(tile) && shape.overlaps_tile(tile))
}

impl Environment {
    pub fn water_at(&self, xy: Vec2) -> Option<WaterSample> {
        let bounds = TileBounds::new(&self.heightmap);
        let tile = bounds.position(f64::from(xy.x), f64::from(xy.y))?;
        Some(
            self.water
                .occupant(bounds, tile)?
                .sample(self.base_height(xy)?),
        )
    }

    pub fn is_water(&self, xy: Vec2) -> bool {
        self.water.occupies(&self.heightmap, xy)
    }

    pub fn water_surface_elevation(&self, xy: Vec2) -> Option<f32> {
        Some(self.water_at(xy)?.surface_z)
    }

    pub fn water_body_kind(&self, xy: Vec2) -> Option<WaterKind> {
        Some(self.water_at(xy)?.kind)
    }

    pub fn water_tiles(&self, chunk: TerrainChunkID) -> impl Iterator<Item = WaterTile> + '_ {
        let bounds = TileBounds::new(&self.heightmap);
        let fresh = self.water.fresh.get(&chunk);
        let sea = if bounds.contains_chunk(chunk) {
            self.water
                .sea
                .get(bounds.index(chunk))
                .filter(|mask| !mask.is_empty())
        } else {
            None
        };
        let end = if fresh.is_some() || sea.is_some() {
            TILES_PER_CHUNK
        } else {
            0
        };
        (0..end).filter_map(move |index| {
            let occupant = match fresh.map(|c| c[index]).filter(|cell| cell.body != 0) {
                Some(cell) => Occupant::fresh(cell, &self.water.bodies[&WaterBodyId(cell.body)]),
                None if sea.is_some_and(|mask| mask.contains(index)) => {
                    Occupant::sea(self.water.sea_level_dm.unwrap())
                }
                None => return None,
            };
            let tile = WaterTileCoord::from_local(chunk, index);
            Some(WaterTile {
                tile,
                sample: occupant.sample(self.base_height(tile.center())?),
            })
        })
    }

    pub fn intersects_water(&self, footprint: &Polygon) -> bool {
        if self.water.fresh.is_empty() && self.water.sea.is_empty() {
            return false;
        }
        let bounds = TileBounds::new(&self.heightmap);
        intersects_tiles(bounds, footprint, |tile| {
            self.water.occupant(bounds, tile).is_some()
        })
    }

    /// True when a stored base node belongs to at least one occupied tile.
    pub fn is_wet_base_node(&self, node: BaseNode) -> bool {
        let bounds = TileBounds::new(&self.heightmap);
        self.heightmap.packed_base_node(node).is_some()
            && incident_tiles(bounds, node).any(|tile| self.water.occupant(bounds, tile).is_some())
    }

    pub fn shoreline_distance(&self, xy: Vec2, max_m: f32) -> Option<f32> {
        if !max_m.is_finite() || max_m < 0.0 {
            return None;
        }
        let bounds = TileBounds::new(&self.heightmap);
        let p = point(xy);
        bounds.position(p.x, p.y)?;
        if self.water.fresh.is_empty() && self.water.sea.is_empty() {
            return None;
        }
        let radius = f64::from(max_m.min(128.0));
        let search = radius + PITCH;
        let mut best = None;
        for tile in bounds.span(
            Vec2d::new(p.x - search, p.y - search),
            Vec2d::new(p.x + search, p.y + search),
        ) {
            if self.water.occupant(bounds, tile).is_none() {
                continue;
            }
            for direction in 0..DIRECTIONS.len() {
                let dry = tile.neighbor(direction);
                if !bounds.contains(dry) || self.water.occupant(bounds, dry).is_some() {
                    continue;
                }
                let edge = point(tile.edge_point(direction));
                let (dx, dy) = DIRECTIONS[direction];
                let half = PITCH * 0.5;
                let closest = Vec2d::new(
                    p.x.clamp(
                        edge.x - f64::from(dy.abs()) * half,
                        edge.x + f64::from(dy.abs()) * half,
                    ),
                    p.y.clamp(
                        edge.y - f64::from(dx.abs()) * half,
                        edge.y + f64::from(dx.abs()) * half,
                    ),
                );
                let d2 = p.distance2(closest);
                if d2 <= radius * radius && best.is_none_or(|prev| d2 < prev) {
                    best = Some(d2);
                }
            }
        }
        best.map(|d2| d2.sqrt() as f32)
    }

    pub fn river_bank_near(
        &self,
        body: WaterBodyId,
        reach_m: u16,
        bank: RiverBank,
        max_reach_delta_m: u16,
    ) -> Option<ShoreContact> {
        let edges = &self.water.banks.get(&body)?[bank.index()];
        let after = edges.partition_point(|edge| edge.reach < reach_m);
        let below = after.checked_sub(1).map(|i| {
            let reach = edges[i].reach;
            edges[edges.partition_point(|e| e.reach < reach)]
        });
        let edge = [below, edges.get(after).copied()]
            .into_iter()
            .flatten()
            .min_by_key(|edge| (edge.reach.abs_diff(reach_m), edge.edge))?;
        if edge.reach.abs_diff(reach_m) > max_reach_delta_m.min(128) {
            return None;
        }
        let wet_tile = edge.tile();
        let direction = edge.direction();
        let record = self.water.bodies.get(&body)?;
        Some(ShoreContact {
            body,
            kind: WaterKind::River,
            surface_z: f32::from(record.level_dm) / 10.0,
            point: wet_tile.edge_point(direction),
            dry_tile: wet_tile.neighbor(direction),
            wet_tile,
            reach_m: Some(edge.reach),
        })
    }

    pub fn raycast_water(&self, ray: Ray3, max_t: f32) -> Option<WaterHit> {
        if !ray.from.is_finite()
            || !ray.dir.is_finite()
            || !max_t.is_finite()
            || max_t < 0.0
            || ray.dir.z == 0.0
            || (self.water.fresh.is_empty() && self.water.sea.is_empty())
        {
            return None;
        }
        let bounds = TileBounds::new(&self.heightmap);
        let axes = [
            (
                f64::from(ray.from.x),
                f64::from(ray.dir.x),
                f64::from(bounds.w) * PITCH,
            ),
            (
                f64::from(ray.from.y),
                f64::from(ray.dir.y),
                f64::from(bounds.h) * PITCH,
            ),
        ];
        let mut start: f64 = 0.0;
        let mut end = f64::from(max_t);
        for (origin, speed, upper) in axes {
            if speed == 0.0 {
                if origin < 0.0 || origin >= upper {
                    return None;
                }
            } else {
                let a = -origin / speed;
                let b = (upper - origin) / speed;
                start = start.max(a.min(b));
                end = end.min(a.max(b));
            }
        }
        if start > end {
            return None;
        }

        let point_at = |t: f64| axes.map(|(p, d, _)| p + d * t);
        let plane_t = |water: Occupant| {
            (f64::from(f32::from(water.level_dm) / 10.0) - f64::from(ray.from.z))
                / f64::from(ray.dir.z)
        };
        let hit = |water: Occupant, t: f64, [x, y]: [f64; 2]| {
            let xy = vec2(x as f32, y as f32);
            let sample = water.sample(self.base_height(xy)?);
            Some(WaterHit {
                point: xy.z(sample.surface_z),
                t: t as f32,
                sample,
            })
        };

        let [x, y] = point_at(start);
        let mut point = [x.clamp(0.0, axes[0].2), y.clamp(0.0, axes[1].2)];
        let mut tile = [0; 2];
        for i in 0..2 {
            let grid = point[i] / PITCH;
            tile[i] = grid.floor() as i16;
            if grid.fract() == 0.0 && axes[i].1 < 0.0 {
                tile[i] -= 1;
            }
        }

        // At a crossing, test the half-open owner at the exact event before
        // moving on. This also covers mixed-sign corner ties and vertical rays.
        for _ in 0..usize::from(bounds.w as u16) + usize::from(bounds.h as u16) + 2 {
            if let Some(owner) = bounds.position(point[0], point[1]) {
                if let Some(water) = self.water.occupant(bounds, owner) {
                    if plane_t(water) == start {
                        return hit(water, start, point);
                    }
                }
            }
            if start >= end || !bounds.contains(WaterTileCoord(tile[0], tile[1])) {
                break;
            }

            let mut crossing = [f64::INFINITY; 2];
            let mut edge = [0.0; 2];
            for i in 0..2 {
                let (origin, speed, _) = axes[i];
                if speed != 0.0 {
                    edge[i] = f64::from(tile[i] + i16::from(speed > 0.0)) * PITCH;
                    crossing[i] = (edge[i] - origin) / speed;
                }
            }
            let next = crossing[0].min(crossing[1]).min(end);
            if let Some(water) = self
                .water
                .occupant(bounds, WaterTileCoord(tile[0], tile[1]))
            {
                let t = plane_t(water);
                if t > start && t < next {
                    return hit(water, t, point_at(t));
                }
            }
            point = point_at(next);
            for i in 0..2 {
                if crossing[i] == next {
                    point[i] = edge[i];
                    tile[i] += if axes[i].1 > 0.0 { 1 } else { -1 };
                }
            }
            start = next;
        }
        None
    }
}

impl WaterState {
    /// Validate every saved authority field before constructing the derived occupancy and banks.
    pub(super) fn validate_and_rebuild(&mut self, map: &Heightmap) -> Result<(), WaterEditError> {
        validate_map_dimensions(map)?;
        if !(1..=TERMINAL_BODY_ID).contains(&self.next_freshwater_id)
            || self
                .bodies
                .keys()
                .any(|id| id.0 == 0 || u32::from(id.0) >= self.next_freshwater_id)
            || (!self.strict && self.next_freshwater_id != 1)
            || (self.revision == 0 && (self.strict || self.sea_level_dm.is_some()))
        {
            return Err(WaterEditError::InvalidInput);
        }
        let allocated = u64::from(self.next_freshwater_id - 1);
        if self.revision < 2 * allocated - self.bodies.len() as u64 {
            return Err(WaterEditError::InvalidInput);
        }
        let bounds = TileBounds::new(map);
        let mut first = BTreeMap::new();
        let mut levels = BTreeMap::new();
        for (&id, body) in &self.bodies {
            levels.insert(id, packed_bed_limit(body.level_dm, MIN_DEPTH)?);
            if let BodyShape::River { stroke, .. } = &body.shape {
                bounds.validate_vertices(stroke.points())?;
            }
        }
        let bed = Bed { map, patch: None };
        for (&chunk, cells) in &self.fresh {
            if !bounds.contains_chunk(chunk) {
                return Err(WaterEditError::OutOfBounds);
            }
            for (index, cell) in cells.iter().enumerate().filter(|(_, cell)| cell.body != 0) {
                let tile = WaterTileCoord::from_local(chunk, index);
                let id = WaterBodyId(cell.body);
                let record = self
                    .bodies
                    .get_mut(&id)
                    .ok_or(WaterEditError::BodyUnavailable(id))?;
                if record.kind() == WaterKind::Lake && cell.reach != 0 {
                    return Err(WaterEditError::InvalidInput);
                }
                if !bed.submerged(tile, levels[&id]) {
                    return Err(WaterEditError::InvalidBed(tile));
                }
                self.cell_count += 1;
                record.cells += 1;
                if record.cells > MAX_FRESHWATER_CANDIDATES {
                    return Err(WaterEditError::Limit);
                }
                first.entry(id).or_insert(tile);
            }
        }

        // Validate and index one body at a time. The scratch bound is per
        // authored body even for a full map or malicious disconnected input.
        for (&id, body) in &self.bodies {
            let first_tile = *first.get(&id).ok_or(WaterEditError::BodyUnavailable(id))?;
            let cells = body.cells;
            match &body.shape {
                BodyShape::River { width_m, .. } => {
                    let raster =
                        body.river
                            .as_ref()
                            .unwrap()
                            .rasterize(bounds, *width_m, &mut Work(0))?;
                    if raster.len() != cells {
                        return Err(WaterEditError::Disconnected(first_tile));
                    }
                    let mut connected = TileSet::default();
                    for ((chunk, index), reach) in raster {
                        let tile = WaterTileCoord::from_local(chunk, index);
                        let cell = self.fresh_cell(tile);
                        if cell != (FreshCell { body: id.0, reach }) {
                            return Err(WaterEditError::InvalidInput);
                        }
                        connected.insert(tile);
                    }
                    validate_connected(connected, cells)?;
                }
                BodyShape::Lake => {
                    let mut coverage = TileSet::default();
                    coverage.insert(first_tile);
                    let mut todo = vec![first_tile];
                    let mut visited = 0;
                    let (mut min, mut max) = (first_tile, first_tile);
                    while let Some(tile) = todo.pop() {
                        visited += 1;
                        min.0 = min.0.min(tile.0);
                        min.1 = min.1.min(tile.1);
                        max.0 = max.0.max(tile.0);
                        max.1 = max.1.max(tile.1);
                        let span = usize::from((max.0 - min.0 + 1) as u16)
                            * usize::from((max.1 - min.1 + 1) as u16);
                        if span > MAX_FRESHWATER_CANDIDATES {
                            return Err(WaterEditError::Limit);
                        }
                        for direction in 0..DIRECTIONS.len() {
                            let neighbor = tile.neighbor(direction);
                            if bounds.contains(neighbor) && self.fresh_cell(neighbor).body == id.0 {
                                let mask = coverage.0.entry(neighbor.chunk()).or_default();
                                let local = neighbor.local_index();
                                if !mask.contains(local) {
                                    mask.insert(local);
                                    todo.push(neighbor);
                                }
                            }
                        }
                    }
                    if visited != cells {
                        return Err(WaterEditError::Disconnected(first_tile));
                    }
                }
            }
        }
        self.finish_loaded_bodies(bounds);

        let prospective =
            rebuild_sea(bounds, bed, |tile| self.fresh_cell(tile), self.sea_level_dm)?;
        if !self.strict && (!prospective.is_empty() || !self.fresh.is_empty()) {
            return Err(WaterEditError::InvalidInput);
        }
        let occupant = |tile: WaterTileCoord| {
            if !bounds.contains(tile) {
                return None;
            }
            let cell = self.fresh_cell(tile);
            if cell.body != 0 {
                let id = WaterBodyId(cell.body);
                return Some(Occupant::fresh(cell, &self.bodies[&id]));
            }
            if sea_contains(&prospective, bounds, tile) {
                return self.sea_level_dm.map(Occupant::sea);
            }
            None
        };
        let mut banks: BTreeMap<WaterBodyId, [Vec<BankEdge>; 2]> = BTreeMap::new();
        for (&chunk, cells) in &self.fresh {
            for (index, cell) in cells.iter().enumerate().filter(|(_, cell)| cell.body != 0) {
                let tile = WaterTileCoord::from_local(chunk, index);
                validate_mouth(tile, occupant)?;
                let id = WaterBodyId(cell.body);
                if let Some(river) = &self.bodies[&id].river {
                    for direction in 0..DIRECTIONS.len() {
                        let dry = tile.neighbor(direction);
                        if bounds.contains(dry) && occupant(dry).is_none() {
                            if let Some(bank) = river.bank(tile, direction, cell.reach) {
                                banks.entry(id).or_default()[bank.index()]
                                    .push(BankEdge::new(tile, direction, cell.reach));
                            }
                        }
                    }
                }
            }
        }
        for edges in banks.values_mut().flatten() {
            edges.sort_unstable();
        }
        self.sea = prospective;
        self.banks = banks;
        Ok(())
    }

    fn finish_loaded_bodies(&mut self, bounds: TileBounds) {
        for (&chunk, cells) in &self.fresh {
            debug_assert!(bounds.contains_chunk(chunk));
            for cell in cells.iter().filter(|cell| cell.body != 0) {
                self.bodies
                    .get_mut(&WaterBodyId(cell.body))
                    .unwrap()
                    .chunks
                    .insert(chunk);
            }
        }
    }
}

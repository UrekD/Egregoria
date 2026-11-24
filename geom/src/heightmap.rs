use crate::{vec2, vec3, Ray3, Vec2, Vec3, AABB, AABB3};
use serde::de::{DeserializeSeed, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};

pub type HeightmapChunkID = (u16, u16);
pub type BaseNode = (usize, usize);

const MAX_HEIGHT_DIFF: f32 = 2048.0;
const HALF_HEIGHT_DIFF: f32 = MAX_HEIGHT_DIFF / 4.0; // Point where half of the precision goes to
const MIN_HEIGHT: f32 = -39.96997;
const MAX_HEIGHT: f32 = MAX_HEIGHT_DIFF - MIN_HEIGHT;

/// Special value for heights_override to indicate that the height is not overridden
pub const NO_OVERRIDE: u16 = 0;

#[derive(Clone)]
pub struct HeightmapChunk<const RESOLUTION: usize, const SIZE: u32> {
    heights: [[u16; RESOLUTION]; RESOLUTION], // TODO: change to RESOLUTION * RESOLUTION when generic_const_exprs is stabilized
    heights_override: [[u16; RESOLUTION]; RESOLUTION],
    max_height: f32,
}

impl<const RESOLUTION: usize, const SIZE: u32> Default for HeightmapChunk<RESOLUTION, SIZE> {
    fn default() -> Self {
        let packed_zero = pack_height(0.0);
        Self {
            heights: [[packed_zero; RESOLUTION]; RESOLUTION],
            heights_override: [[NO_OVERRIDE; RESOLUTION]; RESOLUTION],
            max_height: 0.0,
        }
    }
}

impl<const RESOLUTION: usize, const SIZE: u32> HeightmapChunk<RESOLUTION, SIZE> {
    pub fn new(heights: [[u16; RESOLUTION]; RESOLUTION]) -> Self {
        let mut me = Self {
            heights,
            heights_override: [[NO_OVERRIDE; RESOLUTION]; RESOLUTION],
            max_height: 0.0,
        };
        me.update_max_height();
        me
    }

    #[inline]
    pub fn rect(id: HeightmapChunkID) -> AABB {
        let ll = vec2(id.0 as f32 * SIZE as f32, id.1 as f32 * SIZE as f32);
        let ur = ll + vec2(SIZE as f32, SIZE as f32);
        AABB::new_ll_ur(ll, ur)
    }

    #[inline]
    pub fn id(v: Vec2) -> HeightmapChunkID {
        let x = v.x / SIZE as f32;
        let x = x.clamp(0.0, u16::MAX as f32) as u16;
        let y = v.y / SIZE as f32;
        let y = y.clamp(0.0, u16::MAX as f32) as u16;
        (x, y)
    }

    #[inline]
    pub fn bbox(&self, origin: Vec2) -> AABB3 {
        AABB3::new(
            vec3(origin.x, origin.y, MIN_HEIGHT),
            vec3(
                origin.x + SIZE as f32,
                origin.y + SIZE as f32,
                self.max_height,
            ),
        )
    }

    fn update_max_height(&mut self) {
        for y in 0..RESOLUTION {
            for x in 0..RESOLUTION {
                let h = unsafe { self.height_idx(x, y).unwrap_unchecked() };
                self.max_height = self.max_height.max(h);
            }
        }
    }

    /// assume p is in chunk-space and in-bounds
    #[inline]
    pub fn height_unchecked(&self, p: Vec2) -> f32 {
        let v = p / SIZE as f32;
        let v = v * RESOLUTION as f32;
        self.height_idx(v.x as usize, v.y as usize).unwrap()
    }

    #[inline]
    pub fn height(&self, p: Vec2) -> Option<f32> {
        let v = p / SIZE as f32;
        let v = v * RESOLUTION as f32;
        self.height_idx(v.x as usize, v.y as usize)
    }

    pub fn height_idx(&self, x: usize, y: usize) -> Option<f32> {
        let height_override = *self.heights_override.get(y)?.get(x)?;
        let h = unsafe { *self.heights.get_unchecked(y).get_unchecked(x) };
        if height_override != NO_OVERRIDE && height_override < h {
            return Some(unpack_height(height_override));
        }
        Some(unpack_height(h))
    }

    /// Always returns the normal heightmap height
    fn height_idx_mut(&mut self, x: usize, y: usize) -> Option<&mut u16> {
        self.heights.get_mut(y)?.get_mut(x)
    }

    #[inline]
    pub fn heights(&self) -> &[[u16; RESOLUTION]; RESOLUTION] {
        &self.heights
    }

    #[inline]
    pub fn max_height(&self) -> f32 {
        self.max_height
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Heightmap<const RESOLUTION: usize, const SIZE: u32> {
    chunks: Vec<HeightmapChunk<RESOLUTION, SIZE>>, // chunks is an array of length w * h, indexed with (x + y * w)
    pub w: u16,
    pub h: u16,
}

impl<const RESOLUTION: usize, const SIZE: u32> Heightmap<RESOLUTION, SIZE> {
    pub const CELL_SIZE: f32 = SIZE as f32 / RESOLUTION as f32;
    pub const RESOLUTION: usize = RESOLUTION;
    pub const SIZE: u32 = SIZE;

    pub fn new(w: u16, h: u16) -> Self {
        Self {
            chunks: vec![HeightmapChunk::default(); usize::from(w) * usize::from(h)],
            w,
            h,
        }
    }

    /// Decode the normal heightmap format with a caller-supplied size limit.
    pub fn deserialize_bounded<'de, D: serde::Deserializer<'de>, const LIMIT: u16>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Record<const R: usize, const S: u32, const LIMIT: u16> {
            chunks: BoundedChunks<R, S, LIMIT>,
            w: u16,
            h: u16,
        }
        let Record { chunks, w, h } = Record::<RESOLUTION, SIZE, LIMIT>::deserialize(deserializer)?;
        if w > LIMIT || h > LIMIT {
            return Err(serde::de::Error::custom(
                "heightmap dimensions exceed the supported limit",
            ));
        }
        let expected = usize::from(w) * usize::from(h);
        if chunks.0.len() != expected {
            return Err(serde::de::Error::custom(format_args!(
                "heightmap chunk count mismatch: expected {expected}, got {}",
                chunks.0.len()
            )));
        }
        if chunks.0.iter().any(|chunk| !chunk.max_height.is_finite()) {
            return Err(serde::de::Error::custom(
                "non-finite heightmap chunk maximum",
            ));
        }
        Ok(Self {
            chunks: chunks.0,
            w,
            h,
        })
    }

    pub fn packed_base_node(&self, (x, y): BaseNode) -> Option<u16> {
        if x >= usize::from(self.w) * RESOLUTION || y >= usize::from(self.h) * RESOLUTION {
            return None;
        }
        let chunk = self.get_chunk(((x / RESOLUTION) as u16, (y / RESOLUTION) as u16))?;
        Some(chunk.heights[y % RESOLUTION][x % RESOLUTION])
    }

    /// The stored nodes at the corners of an in-map tile, in LL, LR, UL, UR order.
    pub fn base_tile_nodes(&self, (x, y): BaseNode) -> Option<[BaseNode; 4]> {
        let w = usize::from(self.w) * RESOLUTION;
        let h = usize::from(self.h) * RESOLUTION;
        if x >= w || y >= h {
            return None;
        }
        let right = (x + 1).min(w - 1);
        let upper = (y + 1).min(h - 1);
        Some([(x, y), (right, y), (x, upper), (right, upper)])
    }

    /// Signed bilinear interpolation of the packed base, including the closed outer boundary.
    pub fn base_height(&self, p: Vec2) -> Option<f32> {
        self.sample_base(p, |node| self.packed_base_node(node))
            .map(|(height, _)| height)
    }

    fn sample_base(
        &self,
        p: Vec2,
        mut packed: impl FnMut(BaseNode) -> Option<u16>,
    ) -> Option<(f32, Vec2)> {
        let max = self.bounds().ur;
        if !p.is_finite() || p.x < 0.0 || p.y < 0.0 || p.x > max.x || p.y > max.y {
            return None;
        }
        let grid = p / Self::CELL_SIZE;
        let x = (grid.x as usize).min((usize::from(self.w) * RESOLUTION).checked_sub(1)?);
        let y = (grid.y as usize).min((usize::from(self.h) * RESOLUTION).checked_sub(1)?);
        let nodes = self.base_tile_nodes((x, y))?;
        let [ll, lr, ul, ur] = [
            unpack_height(packed(nodes[0])?),
            unpack_height(packed(nodes[1])?),
            unpack_height(packed(nodes[2])?),
            unpack_height(packed(nodes[3])?),
        ];
        let dx = grid.x - x as f32;
        let dy = grid.y - y as f32;
        let bottom = ll + dx * (lr - ll);
        let top = ul + dx * (ur - ul);
        Some((
            bottom + dy * (top - bottom),
            -vec2((lr - ll) + dy * ((ur - ul) - (lr - ll)), top - bottom),
        ))
    }

    /// Apply a checked journal. A stale original value rejects the whole patch.
    pub fn apply_base_patch(
        &mut self,
        patch: BaseNodePatch,
    ) -> Result<BTreeSet<HeightmapChunkID>, BasePatchError> {
        if patch
            .nodes
            .iter()
            .any(|(&node, &(old, _))| self.packed_base_node(node) != Some(old))
        {
            return Err(BasePatchError::Stale);
        }
        let mut chunks = BTreeSet::new();
        for ((x, y), (old, new)) in patch.nodes {
            if old == new {
                continue;
            }
            *self.height_idx_mut(x, y).unwrap() = new;
            chunks.insert(((x / RESOLUTION) as u16, (y / RESOLUTION) as u16));
        }
        for &id in &chunks {
            let chunk = self.get_chunk_mut(id).unwrap();
            chunk.max_height = 0.0;
            chunk.update_max_height();
        }
        Ok(chunks)
    }

    #[inline]
    pub fn bounds(&self) -> AABB {
        AABB::new_ll_ur(
            vec2(0.0, 0.0),
            vec2(self.w as f32 * SIZE as f32, self.h as f32 * SIZE as f32),
        )
    }

    #[inline]
    fn check_valid(&self, id: HeightmapChunkID) -> bool {
        id.0 < self.w && id.1 < self.h
    }

    #[inline]
    pub fn set_chunk(&mut self, id: HeightmapChunkID, chunk: HeightmapChunk<RESOLUTION, SIZE>) {
        if !self.check_valid(id) {
            return;
        }
        self.chunks[usize::from(id.0) + usize::from(id.1) * usize::from(self.w)] = chunk;
    }

    #[inline]
    pub fn get_chunk(&self, id: HeightmapChunkID) -> Option<&HeightmapChunk<RESOLUTION, SIZE>> {
        if !self.check_valid(id) {
            return None;
        }
        self.chunks
            .get(usize::from(id.0) + usize::from(id.1) * usize::from(self.w))
    }

    pub fn set_override(
        &mut self,
        id: HeightmapChunkID,
        override_heights: [[u16; RESOLUTION]; RESOLUTION],
    ) {
        if !self.check_valid(id) {
            return;
        }
        let chunk = self.get_chunk_mut(id).unwrap();
        chunk.heights_override = override_heights;
        chunk.update_max_height();
    }

    fn get_chunk_mut(
        &mut self,
        id: HeightmapChunkID,
    ) -> Option<&mut HeightmapChunk<RESOLUTION, SIZE>> {
        if !self.check_valid(id) {
            return None;
        }
        self.chunks
            .get_mut(usize::from(id.0) + usize::from(id.1) * usize::from(self.w))
    }

    /// Applies a function to every point in the heightmap in the given bounds
    pub fn apply(&mut self, bounds: AABB, mut f: impl FnMut(Vec3) -> f32) -> Vec<HeightmapChunkID> {
        let ll = bounds.ll / SIZE as f32;
        let ur = bounds.ur / SIZE as f32;
        let ll = vec2(ll.x.floor(), ll.y.floor()).max(Vec2::ZERO);
        let ur = vec2(ur.x.ceil(), ur.y.ceil()).min(vec2(self.w as f32, self.h as f32));

        let mut modified = Vec::with_capacity(((ur.x - ll.x) * (ur.y - ll.y)) as usize);

        for x in ll.x as u16..ur.x as u16 {
            for y in ll.y as u16..ur.y as u16 {
                let id = (x, y);
                let Some(chunk) = self.get_chunk_mut(id) else {
                    continue;
                };
                modified.push(id);
                let corner = vec2(x as f32, y as f32) * SIZE as f32;
                let mut max_height: f32 = 0.0;
                for i in 0..RESOLUTION {
                    for j in 0..RESOLUTION {
                        let p = corner + vec2(j as f32, i as f32) * Self::CELL_SIZE;
                        let h = chunk.heights[i][j];
                        let h = unpack_height(h);
                        max_height = max_height.max(h);
                        if !bounds.contains(p) {
                            continue;
                        }
                        let new_h = f(p.z(h));
                        chunk.heights[i][j] = pack_height(new_h);
                        max_height = max_height.max(new_h);
                    }
                }
                chunk.max_height = max_height;
            }
        }

        modified
    }

    /// Applies a convolution to every point in the heightmap in the given bounds
    /// The function f is called with the point location and the 3x3 grid of heights around it, indexed with (x + y * 3)
    /// This operation is much slower than apply because a copy of the chunk must be done
    pub fn apply_convolution(
        &mut self,
        bounds: AABB,
        mut f: impl FnMut(Vec2, [f32; 9]) -> f32,
    ) -> Vec<HeightmapChunkID> {
        let ll = bounds.ll / SIZE as f32;
        let ur = bounds.ur / SIZE as f32;
        let ll = vec2(ll.x.floor(), ll.y.floor()).max(Vec2::ZERO);
        let ur = vec2(ur.x.ceil(), ur.y.ceil()).min(vec2(self.w as f32, self.h as f32));

        let mut modified = Vec::with_capacity(((ur.x - ll.x) * (ur.y - ll.y)) as usize);

        let mut new_chunks = Vec::with_capacity(((ur.x - ll.x) * (ur.y - ll.y)) as usize);

        let packed_zero = pack_height(0.0);

        for y in ll.y as u16..ur.y as u16 {
            for x in ll.x as u16..ur.x as u16 {
                let id = (x, y);
                modified.push(id);
                let corner = vec2(x as f32, y as f32) * SIZE as f32;
                let mut max_height: f32 = 0.0;
                let mut new_heights = [[packed_zero; RESOLUTION]; RESOLUTION];
                for i in 0..RESOLUTION {
                    for j in 0..RESOLUTION {
                        let p = corner + vec2(j as f32, i as f32) * Self::CELL_SIZE;
                        let mut conv_heights = [0.0; 9];
                        for conv_y in 0..3 {
                            for conv_x in 0..3 {
                                conv_heights[conv_x + conv_y * 3] = self
                                    .height_idx(
                                        (x as usize * RESOLUTION + j + conv_x).saturating_sub(1),
                                        (y as usize * RESOLUTION + i + conv_y).saturating_sub(1),
                                    )
                                    .unwrap_or(0.0);
                            }
                        }
                        let mut new_h = conv_heights[4];
                        if bounds.contains(p) {
                            new_h = f(p, conv_heights).clamp(MIN_HEIGHT, MAX_HEIGHT);
                        }
                        new_heights[i][j] = pack_height(new_h);
                        max_height = max_height.max(new_h);
                    }
                }
                new_chunks.push((id, new_heights, max_height));
            }
        }

        for (id, new_heights, max_height) in new_chunks {
            let chunk = self.get_chunk_mut(id).unwrap();
            chunk.heights = new_heights;
            chunk.max_height = max_height;
        }

        modified
    }

    pub fn chunks(
        &self,
    ) -> impl Iterator<Item = (HeightmapChunkID, &HeightmapChunk<RESOLUTION, SIZE>)> + '_ {
        self.chunks.iter().enumerate().map(move |(i, chunk)| {
            let w = usize::from(self.w);
            (((i % w) as u16, (i / w) as u16), chunk)
        })
    }

    pub fn covered_chunks(&self, bounds: AABB) -> impl Iterator<Item = HeightmapChunkID> {
        let ll = bounds.ll / SIZE as f32;
        let ur = bounds.ur / SIZE as f32;
        let ll = vec2(ll.x.floor(), ll.y.floor()).max(Vec2::ZERO);
        let ur = vec2(ur.x.ceil(), ur.y.ceil()).min(vec2(self.w as f32, self.h as f32));

        (ll.y as u16..ur.y as u16)
            .flat_map(move |y| (ll.x as u16..ur.x as u16).map(move |x| (x, y)))
    }

    /// Returns height at any point using the cell to the bottom left of the point
    pub fn height_nearest(&self, p: Vec2) -> Option<f32> {
        let cell = HeightmapChunk::<RESOLUTION, SIZE>::id(p);

        self.get_chunk(cell)
            .and_then(|chunk| chunk.height(p - vec2(cell.0 as f32, cell.1 as f32) * SIZE as f32))
    }

    /// get height by actual cell position
    fn height_idx(&self, x: usize, y: usize) -> Option<f32> {
        let chunkx = x / RESOLUTION;
        let chunky = y / RESOLUTION;

        let cellx = x % RESOLUTION;
        let celly = y % RESOLUTION;

        let chunk = self.get_chunk((chunkx as u16, chunky as u16))?;

        // Safety: modulo RESOLUTION
        unsafe { Some(chunk.height_idx(cellx, celly).unwrap_unchecked()) }
    }

    /// Always returns the normal heightmap height
    fn height_idx_mut(&mut self, x: usize, y: usize) -> Option<&mut u16> {
        let chunkx = x / RESOLUTION;
        let chunky = y / RESOLUTION;

        let cellx = x % RESOLUTION;
        let celly = y % RESOLUTION;

        let chunk = self.get_chunk_mut((chunkx as u16, chunky as u16))?;

        // Safety: modulo RESOLUTION
        unsafe { Some(chunk.height_idx_mut(cellx, celly).unwrap_unchecked()) }
    }

    /// Returns height at any point using bilinear interpolation
    pub fn height(&self, p: Vec2) -> Option<f32> {
        let exact = self.height_nearest(p);
        if let (Some(ll), Some(lr), Some(ul), Some(ur)) = (
            exact,
            self.height_nearest(p + Vec2::x(Self::CELL_SIZE)),
            self.height_nearest(p + Vec2::y(Self::CELL_SIZE)),
            self.height_nearest(p + vec2(Self::CELL_SIZE, Self::CELL_SIZE)),
        ) {
            let v = (p / Self::CELL_SIZE).fract();

            let h01 = ll + v.x * (lr - ll);
            let h23 = ul + v.x * (ur - ul);

            return Some(h01 + v.y * (h23 - h01));
        }
        exact
    }

    /// Returns height and gradient at any point using bilinear interpolation
    /// The gradient is the vector pointing in the direction of the steepest slope (downwards)
    pub fn height_gradient(&self, p: Vec2) -> Option<(f32, Vec2)> {
        let exact = self.height_nearest(p);
        if let (Some(ll), Some(lr), Some(ul), Some(ur)) = (
            exact,
            self.height_nearest(p + Vec2::x(Self::CELL_SIZE)),
            self.height_nearest(p + Vec2::y(Self::CELL_SIZE)),
            self.height_nearest(p + vec2(Self::CELL_SIZE, Self::CELL_SIZE)),
        ) {
            let x = (p.x / Self::CELL_SIZE).fract();
            let y = (p.y / Self::CELL_SIZE).fract();

            let h01 = ll + x * (lr - ll);
            let h23 = ul + x * (ur - ul);

            let height = h01 + y * (h23 - h01);
            let gradient = -vec2(
                (lr - ll) + y * ((ur - ul) - (lr - ll)),
                (ul - ll) + x * ((ur - lr) - (ul - ll)),
            );

            return Some((height, gradient));
        }
        exact.zip(Some(Vec2::ZERO))
    }

    /// Casts a ray on the heightmap, returning the point of intersection and the normal at that point
    /// We assume height is between [-40.0; 2008]
    pub fn raycast(&self, ray: Ray3) -> Option<(Vec3, Vec3)> {
        if !ray.dir.is_finite() {
            return None;
        }
        // Let's build an iterator over the chunks that intersect the ray (from nearest to furthest)
        let start = ray.from.xy() / SIZE as f32;
        let end = start + ray.dir.xy().normalize() * self.w.max(self.h) as f32 * 2.0;

        let diff = end - start;
        let l = diff.mag();
        let speed = diff / l;

        let mut t = 0.0;

        let mut cur = start;

        let intersecting_chunks = std::iter::once((start.x as isize, start.y as isize))
            .chain(std::iter::from_fn(|| {
                let x = cur.x - cur.x.floor();
                let y = cur.y - cur.y.floor();

                let t_x;
                let t_y;

                if speed.x >= 0.0 {
                    t_x = (1.0 - x) / speed.x;
                } else {
                    t_x = -x / speed.x;
                }
                if speed.y >= 0.0 {
                    t_y = (1.0 - y) / speed.y;
                } else {
                    t_y = -y / speed.y;
                }

                let min_t = t_x.min(t_y) + 0.0001;
                t += min_t;
                #[allow(clippy::neg_cmp_op_on_partial_ord)]
                // reverse the condition to avoid infinite loop in case of NaN
                if !(t < l) {
                    return None;
                }
                cur += min_t * speed;
                Some((cur.x as isize, cur.y as isize))
            }))
            .filter(|&(x, y)| x < self.w as isize && y < self.h as isize && x >= 0 && y >= 0)
            .filter_map(|(x, y)| {
                let chunk_id = (x as u16, y as u16);
                let corner = vec2(x as f32, y as f32) * SIZE as f32;
                let mut bbox = self.get_chunk(chunk_id)?.bbox(corner);
                bbox.ll.z -= Self::CELL_SIZE; // give a bit of margin to avoid missing intersections when the heightmap is at the lowest
                let (t_min, t_max) = bbox.raycast(ray)?;
                Some((t_min, t_max))
            });

        // Now within those chunks, let's try to find the intersection point
        // h < t * ray.dir.z + ray.from.z
        for (t_min, t_max) in intersecting_chunks {
            let mut t = t_min;
            let t_step = Self::CELL_SIZE;

            loop {
                let p = ray.from + ray.dir * t;
                let Some(h) = self.height(p.xy()) else {
                    if t >= t_max {
                        break;
                    }
                    t += t_step;
                    continue;
                };
                if p.z < h {
                    // we found a good candidate but we're not there yet
                    // we still need to do one last binary search
                    // to find the bilinear-filtered-corrected location

                    let c = binary_search(0.0, 1.0, |c| {
                        let t_test = t - t_step * 2.0 * (1.0 - c);
                        let p = ray.from + ray.dir * t_test;
                        let Some(h) = self.height(p.xy()) else {
                            return false;
                        };
                        p.z < h
                    });
                    let t = t - t_step * 2.0 * (1.0 - c);

                    return Some((ray.from + ray.dir * t, vec3(0.0, 0.0, 1.0)));
                }
                if t >= t_max {
                    break;
                }
                t += t_step;
            }
        }

        None
    }
}

/// Does a binary search on the interval [min; max] to find the first value for which f returns true
fn binary_search(min: f32, max: f32, mut f: impl FnMut(f32) -> bool) -> f32 {
    let mut min = min;
    let mut max = max;
    let mut mid = min + (max - min) * 0.5;
    loop {
        if f(mid) {
            max = mid;
        } else {
            min = mid;
        }
        let new_mid = min + (max - min) * 0.5;
        if (new_mid - mid).abs() < 0.0001 {
            break;
        }
        mid = new_mid;
    }
    mid
}

const HALF_U16: u16 = u16::MAX >> 1;

/// Largest packed base sample whose decoded height does not exceed the requested ceiling.
pub fn pack_base_height_at_most(height: f64) -> Option<u16> {
    if !height.is_finite() || height < f64::from(MIN_HEIGHT) {
        return None;
    }
    let mut packed = pack_height(height as f32);
    while f64::from(unpack_height(packed)) > height {
        packed -= 1;
    }
    while packed < u16::MAX && f64::from(unpack_height(packed + 1)) <= height {
        packed += 1;
    }
    Some(packed)
}

pub fn pack_height(height: f32) -> u16 {
    let height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);

    let height_off = height - MIN_HEIGHT;

    if height < HALF_HEIGHT_DIFF {
        return (height_off / HALF_HEIGHT_DIFF * HALF_U16 as f32) as u16;
    }

    let height_off = height_off - HALF_HEIGHT_DIFF;
    (HALF_U16 as f32 + height_off / HALF_HEIGHT_DIFF * HALF_U16 as f32) as u16
}

pub fn unpack_height(height: u16) -> f32 {
    if height < HALF_U16 {
        return MIN_HEIGHT + height as f32 / HALF_U16 as f32 * HALF_HEIGHT_DIFF;
    }

    let height = height - HALF_U16;
    MIN_HEIGHT + HALF_HEIGHT_DIFF + height as f32 / HALF_U16 as f32 * HALF_HEIGHT_DIFF
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BasePatchError {
    Bounds,
    Limit,
    Stale,
}

/// A bounded journal of packed base writes. Originals are preserved for atomic application.
#[derive(Debug, Clone)]
pub struct BaseNodePatch {
    limit: usize,
    nodes: BTreeMap<BaseNode, (u16, u16)>,
}

impl BaseNodePatch {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            nodes: BTreeMap::new(),
        }
    }

    pub fn changes(&self) -> impl Iterator<Item = (BaseNode, u16, u16)> + '_ {
        self.nodes
            .iter()
            .filter_map(|(&node, &(old, new))| (old != new).then_some((node, old, new)))
    }

    pub fn staged_node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn stage<const R: usize, const S: u32>(
        &mut self,
        map: &Heightmap<R, S>,
        node: BaseNode,
        value: u16,
    ) -> Result<(), BasePatchError> {
        if let Some((_, new)) = self.nodes.get_mut(&node) {
            *new = value;
            return Ok(());
        }
        let old = map.packed_base_node(node).ok_or(BasePatchError::Bounds)?;
        if old != value {
            if self.nodes.len() >= self.limit {
                return Err(BasePatchError::Limit);
            }
            self.nodes.insert(node, (old, value));
        }
        Ok(())
    }

    pub fn packed_base_node<const R: usize, const S: u32>(
        &self,
        map: &Heightmap<R, S>,
        node: BaseNode,
    ) -> Option<u16> {
        self.nodes
            .get(&node)
            .map(|&(_, new)| new)
            .or_else(|| map.packed_base_node(node))
    }

    pub fn base_height<const R: usize, const S: u32>(
        &self,
        map: &Heightmap<R, S>,
        point: Vec2,
    ) -> Option<f32> {
        map.sample_base(point, |node| self.packed_base_node(map, node))
            .map(|(height, _)| height)
    }
}

struct BoundedChunks<const R: usize, const S: u32, const LIMIT: u16>(Vec<HeightmapChunk<R, S>>);

impl<'de, const R: usize, const S: u32, const LIMIT: u16> Deserialize<'de>
    for BoundedChunks<R, S, LIMIT>
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_seq(Self(Vec::new()))
    }
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    //    #[test]
    fn find_pack_zero() {
        let mut min_height = -40.0;

        for _ in 0..10000 {
            min_height += 0.00001;

            let height_off = 0.0 - min_height;

            let packed = (height_off / HALF_HEIGHT_DIFF * HALF_U16 as f32) as u16;

            let unpacked = min_height + packed as f32 / HALF_U16 as f32 * HALF_HEIGHT_DIFF;

            if unpacked >= 0.0 {
                println!("{} {}", min_height, unpacked);
            }
        }
    }
}

impl<const RESOLUTION: usize, const SIZE: u32> Serialize for HeightmapChunk<RESOLUTION, SIZE> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(1 + RESOLUTION * RESOLUTION * 2))?;
        seq.serialize_element(&self.max_height)?;
        Self::encode_delta(&mut seq, &self.heights)?;
        Self::encode_delta(&mut seq, &self.heights_override)?;
        seq.end()
    }
}

impl<const RESOLUTION: usize, const SIZE: u32> HeightmapChunk<RESOLUTION, SIZE> {
    fn encode_delta<S: SerializeSeq>(
        seq: &mut S,
        heights: &[[u16; RESOLUTION]; RESOLUTION],
    ) -> Result<(), S::Error> {
        let mut last = 0;
        for row in heights {
            for &height in row {
                let delta = height.wrapping_sub(last);
                seq.serialize_element(&delta)?;
                last = height;
            }
        }
        Ok(())
    }
}

impl<'de, const RESOLUTION: usize, const SIZE: u32> Deserialize<'de>
    for HeightmapChunk<RESOLUTION, SIZE>
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (heights, heights_override, max_height) =
            deserializer.deserialize_seq(HeightmapChunkVisitor::<RESOLUTION>)?;
        Ok(Self {
            heights,
            heights_override,
            max_height,
        })
    }
}

impl<'de, const R: usize, const S: u32, const LIMIT: u16> Visitor<'de>
    for BoundedChunks<R, S, LIMIT>
{
    type Value = Self;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "at most {} terrain chunks", usize::from(LIMIT).pow(2))
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<Self, A::Error> {
        use serde::de::Error;
        let limit = usize::from(LIMIT).pow(2);
        if let Some(len) = seq.size_hint().filter(|&len| len > limit) {
            return Err(Error::invalid_length(len, &self));
        }
        while self.0.len() < limit {
            match seq.next_element()? {
                Some(chunk) => self.0.push(chunk),
                None => return Ok(self),
            }
        }
        struct Overflow;
        impl<'de> DeserializeSeed<'de> for Overflow {
            type Value = ();
            fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
                Err(Error::custom("terrain chunk limit exceeded"))
            }
        }
        seq.next_element_seed(Overflow)?;
        Ok(self)
    }
}

struct HeightmapChunkVisitor<const RESOLUTION: usize>;

impl<'de, const RESOLUTION: usize> Visitor<'de> for HeightmapChunkVisitor<RESOLUTION> {
    type Value = (
        [[u16; RESOLUTION]; RESOLUTION],
        [[u16; RESOLUTION]; RESOLUTION],
        f32,
    );

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a sequence of floats")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let len = seq.size_hint().unwrap_or(1 + RESOLUTION * RESOLUTION * 2);
        if len != 1 + RESOLUTION * RESOLUTION * 2 {
            return Err(serde::de::Error::invalid_length(
                len,
                &"not enough elements to decode map chunk",
            ));
        }
        let max_height = seq
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(0, &""))?;
        let mut heights = [[0; RESOLUTION]; RESOLUTION];
        let mut heights_override = [[0; RESOLUTION]; RESOLUTION];

        Self::decode_delta(&mut seq, &mut heights)?;
        Self::decode_delta(&mut seq, &mut heights_override)?;

        Ok((heights, heights_override, max_height))
    }
}

impl<const RESOLUTION: usize> HeightmapChunkVisitor<RESOLUTION> {
    fn decode_delta<'de, A: SeqAccess<'de>>(
        seq: &mut A,
        heights: &mut [[u16; RESOLUTION]; RESOLUTION],
    ) -> Result<(), A::Error> {
        let mut last = 0;
        for row in heights {
            for height in row {
                let delta: u16 = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &""))?;
                let packed = delta.wrapping_add(last);
                *height = packed;
                last = packed;
            }
        }
        Ok(())
    }
}

mod erosion {
    use crate::heightmap::{unpack_height, BaseNode, BaseNodePatch, BasePatchError, MIN_HEIGHT};
    use crate::{
        pack_height, vec2, Heightmap, HeightmapChunk, HeightmapChunkID, Radians, Vec2, AABB,
    };
    use std::collections::BTreeSet;
    use std::ops::Div;

    // taken from https://github.com/SebLague/Hydraulic-Erosion/blob/master/Assets/Scripts/Erosion.cs
    // Copyright (c) 2019 Sebastian Lague
    // 2..8
    const EROSION_RADIUS: isize = 3;
    // 0..1
    const INERTIA: f32 = 0.1; // At zero, water will instantly change direction to flow downhill. At 1, water will never change direction.
    const SEDIMENT_CAPACITY_FACTOR: f32 = 1.0; // Multiplier for how much sediment a droplet can carry
    const MIN_SEDIMENT_CAPACITY: f32 = 0.003; // Used to prevent carry capacity getting too close to zero on flatter heightmap

    // 0..1
    const ERODE_SPEED: f32 = 0.3;

    // 0..1
    const DEPOSIT_SPEED: f32 = 0.3;
    // 0..1
    const EVAPORATE_SPEED: f32 = 0.01;
    const GRAVITY: f32 = 1.0;
    const MAX_DROPLET_LIFETIME: usize = 50;

    const INITIAL_WATER_VOLUME: f32 = 1.0;
    const INITIAL_SPEED: f32 = 1.0;

    enum ErosionTerrain<'a, const R: usize, const S: u32> {
        Live(&'a mut Heightmap<R, S>, BTreeSet<HeightmapChunkID>),
        Staged(&'a Heightmap<R, S>, &'a mut BaseNodePatch),
    }

    impl<const R: usize, const S: u32> Heightmap<R, S> {
        /// Conservative work for each particle, including brush writes and reads.
        pub const EROSION_WORK_PER_PARTICLE: usize = MAX_DROPLET_LIFETIME * 64;

        pub fn erode(
            &mut self,
            bounds: AABB,
            n_particles: usize,
            randgen: impl FnMut() -> f32,
        ) -> Vec<HeightmapChunkID> {
            let mut target = ErosionTerrain::Live(self, BTreeSet::new());
            target.erode(bounds, n_particles, randgen).unwrap();
            let ErosionTerrain::Live(map, changed) = target else {
                unreachable!()
            };
            for &id in &changed {
                if let Some(chunk) = map.get_chunk_mut(id) {
                    chunk.update_max_height();
                }
            }
            changed.into_iter().collect()
        }

        pub fn stage_base_erosion(
            &self,
            patch: &mut BaseNodePatch,
            bounds: AABB,
            n_particles: usize,
            randgen: impl FnMut() -> f32,
        ) -> Result<(), BasePatchError> {
            ErosionTerrain::Staged(self, patch).erode(bounds, n_particles, randgen)
        }
    }

    impl<const R: usize, const S: u32> ErosionTerrain<'_, R, S> {
        fn map(&self) -> &Heightmap<R, S> {
            match self {
                Self::Live(map, _) => map,
                Self::Staged(map, _) => map,
            }
        }

        fn height_gradient(&self, point: Vec2) -> Option<(f32, Vec2)> {
            match self {
                Self::Live(map, _) => map.height_gradient(point),
                Self::Staged(map, patch) => {
                    map.sample_base(point, |node| patch.packed_base_node(map, node))
                }
            }
        }

        fn height(&self, point: Vec2) -> Option<f32> {
            match self {
                Self::Live(map, _) => map.height(point),
                Self::Staged(map, patch) => patch.base_height(map, point),
            }
        }

        fn height_idx(&self, (x, y): BaseNode) -> Option<f32> {
            match self {
                Self::Live(map, _) => map.height_idx(x, y),
                Self::Staged(map, patch) => patch.packed_base_node(map, (x, y)).map(unpack_height),
            }
        }

        fn add_height(&mut self, node: BaseNode, delta: f32) -> Result<(), BasePatchError> {
            match self {
                Self::Live(map, _) => {
                    if let Some(value) = map.height_idx_mut(node.0, node.1) {
                        *value = pack_height(unpack_height(*value) + delta);
                    }
                }
                Self::Staged(map, patch) => {
                    if delta != 0.0 {
                        if let Some(value) = patch.packed_base_node(map, node) {
                            patch.stage(map, node, pack_height(unpack_height(value) + delta))?;
                        }
                    }
                }
            }
            Ok(())
        }

        fn visit_droplet(&mut self, point: Vec2) {
            if let Self::Live(_, changed) = self {
                changed.insert(HeightmapChunk::<R, S>::id(point));
            }
        }

        fn erode(
            &mut self,
            bounds: AABB,
            n_particles: usize,
            mut randgen: impl FnMut() -> f32,
        ) -> Result<(), BasePatchError> {
            let width = f32::from(self.map().w) * R as f32;
            let height_nodes = f32::from(self.map().h) * R as f32;
            let pitch = Heightmap::<R, S>::CELL_SIZE;
            let mut erosion_brush_total = 0.0;
            for y in -EROSION_RADIUS..=EROSION_RADIUS {
                for x in -EROSION_RADIUS..=EROSION_RADIUS {
                    let dist2 = x * x + y * y;
                    if dist2 >= EROSION_RADIUS * EROSION_RADIUS {
                        continue;
                    }
                    erosion_brush_total += 1.0 - f32::sqrt(dist2 as f32) / EROSION_RADIUS as f32;
                }
            }

            for _ in 0..n_particles {
                // Create water droplet at random point in bounds, in a circle

                let d = randgen().powf(0.8) * bounds.w() * 0.5;
                let angle = Radians(randgen() * std::f32::consts::TAU);
                let pos = bounds.center() + angle.vec2() * d;

                let mut pos = pos / pitch;

                let mut dir = Vec2::ZERO;

                let mut speed = INITIAL_SPEED;
                let mut water = INITIAL_WATER_VOLUME;
                let mut sediment = 0.0;

                for _ in 0..MAX_DROPLET_LIFETIME {
                    let cell_offset = pos - pos.floor();

                    // Calculate droplet's height and direction of flow with bilinear interpolation of surrounding heights
                    let Some((height, gradient)) = self.height_gradient(pos * pitch) else {
                        break;
                    };

                    // Update the droplet's direction and position (move position 1 unit regardless of speed)
                    dir = Vec2::lerp(gradient, dir, INERTIA);

                    // Normalize direction
                    let dl = dir.mag();
                    if dl < f32::EPSILON {
                        dir = Vec2::from_angle(Radians(randgen() * std::f32::consts::TAU));
                    } else {
                        dir /= dl;
                    }

                    // Stop simulating droplet if it's not moving or has flowed over edge of map
                    if (dir.x == 0.0 && dir.y == 0.0)
                        || pos.x < 0.0
                        || pos.x >= width
                        || pos.y < 0.0
                        || pos.y >= height_nodes
                    {
                        break;
                    }

                    // Find the droplet's new height and calculate the deltaHeight
                    let new_height = self.height((pos + dir) * pitch).unwrap_or(0.0);
                    let delta_height = (new_height - height) / pitch;

                    // Calculate the droplet's sediment capacity (higher when moving fast down a slope and contains lots of water)
                    let sediment_capacity =
                        (-delta_height * speed * water * SEDIMENT_CAPACITY_FACTOR)
                            .max(MIN_SEDIMENT_CAPACITY);

                    // If carrying more sediment than capacity, or if flowing uphill:
                    if sediment > sediment_capacity || delta_height > 0.0 {
                        // If moving uphill (delta_height > 0) try fill up to the current height, otherwise deposit a fraction of the excess sediment
                        let amount_to_deposit = if delta_height > 0.0 {
                            f32::min(delta_height, sediment)
                        } else {
                            (sediment - sediment_capacity) * DEPOSIT_SPEED
                        };
                        sediment -= amount_to_deposit;

                        // Add the sediment to the four nodes of the current cell using bilinear interpolation
                        // Deposition is not distributed over a radius (like erosion) so that it can fill small pits
                        self.visit_droplet(pos * pitch);
                        let amount_to_deposit = amount_to_deposit * pitch;
                        let (x, y) = (pos.x as usize, pos.y as usize);
                        self.add_height(
                            (x, y),
                            amount_to_deposit * (1.0 - cell_offset.x) * (1.0 - cell_offset.y),
                        )?;
                        self.add_height(
                            (x + 1, y),
                            amount_to_deposit * cell_offset.x * (1.0 - cell_offset.y),
                        )?;
                        self.add_height(
                            (x, y + 1),
                            amount_to_deposit * (1.0 - cell_offset.x) * cell_offset.y,
                        )?;
                        self.add_height(
                            (x + 1, y + 1),
                            amount_to_deposit * cell_offset.x * cell_offset.y,
                        )?;
                    } else {
                        // Erode a fraction of the droplet's current carry capacity.
                        // Clamp the erosion to the change in height so that it doesn't dig a hole in the heightmap behind the droplet
                        let amount_to_erode =
                            f32::min((sediment_capacity - sediment) * ERODE_SPEED, -delta_height);

                        // Use erosion brush to erode from all nodes inside the droplet's erosion radius
                        self.visit_droplet(pos * pitch);

                        for erode_y in -EROSION_RADIUS..=EROSION_RADIUS {
                            for erode_x in -EROSION_RADIUS..=EROSION_RADIUS {
                                let dist2 = erode_x * erode_x + erode_y * erode_y;
                                if dist2 >= EROSION_RADIUS * EROSION_RADIUS {
                                    continue;
                                }
                                let w = (1.0 - f32::sqrt(dist2 as f32) / EROSION_RADIUS as f32)
                                    / erosion_brush_total;

                                let pos_radius = pos + vec2(erode_x as f32, erode_y as f32);

                                let weighed_erode_amount = amount_to_erode * w;

                                let node = (pos_radius.x as usize, pos_radius.y as usize);
                                let delta_sediment = (self.height_idx(node).unwrap_or(0.0)
                                    - MIN_HEIGHT)
                                    .div(pitch)
                                    .min(weighed_erode_amount);

                                self.add_height(node, -(delta_sediment * pitch))?;

                                sediment += delta_sediment;
                            }
                        }
                    }

                    // Update droplet's speed and water content
                    speed = f32::sqrt(speed * speed - delta_height * GRAVITY) * 0.98;
                    water *= 1.0 - EVAPORATE_SPEED;
                    pos += dir;
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod water_base_tests {
    use super::*;

    #[test]
    fn water_base_uses_real_seam_and_clamps_each_outer_axis() {
        let mut map = Heightmap::<2, 32>::new(2, 1);
        map.set_chunk((0, 0), HeightmapChunk::new([[pack_height(4.0); 2]; 2]));
        map.set_chunk(
            (1, 0),
            HeightmapChunk::new([[pack_height(-4.0), pack_height(8.0)]; 2]),
        );
        map.set_override((1, 0), [[pack_height(-20.0); 2]; 2]);
        assert!(map.base_height(vec2(24.0, 30.0)).unwrap().abs() < 0.03);
        assert!((map.base_height(vec2(64.0, 32.0)).unwrap() - 8.0).abs() < 0.03);
        assert!(map.base_height(vec2(-1.0, 0.0)).is_none());

        let mut patch = BaseNodePatch::new(2);
        patch.stage(&map, (2, 1), pack_height(-12.0)).unwrap();
        assert!(patch.base_height(&map, vec2(24.0, 32.0)).unwrap() < -3.9);
        assert!(map.base_height(vec2(24.0, 32.0)).unwrap().abs() < 0.03);
        map.apply_base_patch(patch).unwrap();
        assert!(map.base_height(vec2(24.0, 32.0)).unwrap() < -3.9);
    }

    #[test]
    fn water_base_erosion_stages_neighbors_on_both_sides_of_a_seam() {
        let mut map = Heightmap::<32, 512>::new(2, 1);
        map.apply(map.bounds(), |p| 25.0 - p.x * 0.04);
        let bounds = AABB::centered(vec2(508.0, 256.0), Vec2::splat(32.0));
        let mut patch = BaseNodePatch::new(4096);
        map.stage_base_erosion(&mut patch, bounds, 2, || 0.0)
            .unwrap();
        assert!(patch.changes().any(|((x, _), _, _)| x == 31));
        assert!(patch.changes().any(|((x, _), _, _)| x == 32));
        for (node, old, new) in patch.changes() {
            assert_eq!(map.packed_base_node(node), Some(old));
            assert_eq!(patch.packed_base_node(&map, node), Some(new));
        }
        assert_eq!(
            map.apply_base_patch(patch).unwrap(),
            BTreeSet::from([(0, 0), (1, 0)])
        );
    }

    #[test]
    fn water_base_patch_rejects_stale_and_excess_nodes_atomically() {
        let mut map = Heightmap::<2, 32>::new(2, 1);
        let mut patch = BaseNodePatch::new(1);
        patch.stage(&map, (1, 0), pack_height(4.0)).unwrap();
        assert_eq!(
            patch.stage(&map, (2, 0), pack_height(4.0)),
            Err(BasePatchError::Limit)
        );
        let mut other = BaseNodePatch::new(1);
        other.stage(&map, (1, 0), pack_height(2.0)).unwrap();
        map.apply_base_patch(other).unwrap();
        assert_eq!(map.apply_base_patch(patch), Err(BasePatchError::Stale));
        assert_eq!(map.packed_base_node((1, 0)), Some(pack_height(2.0)));
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use engine::{FrameContext, MeshBuilder, MeshVertex, Water, WaterOverviewAtlas};
use geom::{vec2, Intersect3, LinearColor, AABB3};
use simulation::map::{
    Environment, Heightmap, Map, MapSubscriber, TerrainChunkID, UpdateType, WaterKind, WaterTile,
};

const RES: usize = Heightmap::RESOLUTION;
pub(crate) const WATER_TILE_PITCH: f32 = Heightmap::SIZE as f32 / RES as f32;
pub(crate) const WATER_OVERVIEW_POOL: usize = 4;
const OVERVIEW_PITCH: f32 = WATER_TILE_PITCH * WATER_OVERVIEW_POOL as f32;

pub(crate) fn water_color(kind: WaterKind) -> LinearColor {
    match kind {
        WaterKind::Sea => LinearColor::new(0.035, 0.19, 0.43, 1.0),
        WaterKind::River => LinearColor::new(0.035, 0.39, 0.51, 1.0),
        WaterKind::Lake => LinearColor::new(0.045, 0.29, 0.32, 1.0),
    }
}

pub(super) struct WaterRender {
    updates: MapSubscriber,
    revision: Option<u64>,
    level_range: Option<(f32, f32)>,
    pending: BTreeSet<TerrainChunkID>,
    meshes: BTreeMap<TerrainChunkID, (AABB3, Water)>,
    overview: Option<Arc<WaterOverviewAtlas>>,
}

impl WaterRender {
    pub(super) fn new(map: &Map) -> Self {
        Self {
            updates: map.subscribe(UpdateType::Water),
            revision: None,
            level_range: None,
            pending: BTreeSet::new(),
            meshes: BTreeMap::new(),
            overview: None,
        }
    }

    pub(super) fn draw(&mut self, env: &Environment, ctx: &mut FrameContext<'_>) {
        let revision = env.water_revision();
        let dirty = self.updates.take_updated_chunks();
        let cleared = self.updates.take_cleared();
        if cleared || self.revision.is_none() {
            self.reset(env);
        } else if self.revision != Some(revision) {
            let mut dirty = dirty.peekable();
            if dirty.peek().is_none() {
                // A terrain-only fixture can use W1's Environment methods before publishing.
                self.reset(env);
            }
            for bucket in dirty {
                for chunk in bucket.convert() {
                    self.meshes.remove(&chunk);
                    if env.get_chunk(chunk).is_some() {
                        self.pending.insert(chunk);
                    }
                }
            }
        }
        if self.revision != Some(revision) || cleared {
            self.level_range = env
                .sea_level_dm()
                .into_iter()
                .chain(env.freshwater_bodies().map(|(_, _, level)| level))
                .map(|level| f32::from(level) / 10.0)
                .fold(None, |range: Option<(f32, f32)>, z| {
                    Some(range.map_or((z, z), |(min, max)| (min.min(z), max.max(z))))
                });
        }
        self.revision = Some(revision);
        let Some((low, high)) = self.level_range else {
            self.pending.clear();
            self.meshes.clear();
            return;
        };

        let visible: Vec<_> = self
            .pending
            .iter()
            .copied()
            .filter(|chunk| {
                let xy = chunk.bbox();
                let bounds = AABB3 {
                    ll: xy.ll.z(low),
                    ur: xy.ur.z(high),
                };
                ctx.gfx.frustrum.intersects(&bounds)
            })
            .collect();
        let world_origin = env.bounds().ll;
        for chunk in visible {
            self.pending.remove(&chunk);
            let mut mesh = MeshBuilder::<false>::new_without_mat();
            let mut bounds: Option<AABB3> = None;
            let origin = chunk.bbox().ll;
            for_each_rect(env.water_tiles(chunk), |rect| {
                let ll = origin + vec2(rect.x as f32, rect.y as f32) * WATER_TILE_PITCH;
                let ur = ll + vec2(rect.width as f32, rect.height as f32) * WATER_TILE_PITCH;
                let z = rect.surface.z;
                let rect_bounds = AABB3 {
                    ll: ll.z(z),
                    ur: ur.z(z),
                };
                bounds = Some(bounds.map_or(rect_bounds, |bounds| bounds.union(rect_bounds)));
                let color = water_color(rect.surface.kind).into();
                let vertex = |xy: geom::Vec2| MeshVertex {
                    position: xy.z(z).into(),
                    normal: geom::Vec3::Z,
                    color,
                    uv: ((xy - world_origin) / OVERVIEW_PITCH).into(),
                    ..MeshVertex::default()
                };
                mesh.extend(
                    None,
                    &[
                        vertex(ll),
                        vertex(vec2(ur.x, ll.y)),
                        vertex(ur),
                        vertex(vec2(ll.x, ur.y)),
                    ],
                    &[0, 1, 2, 2, 3, 0],
                );
            });
            if let Some(bounds) = bounds {
                self.meshes.insert(chunk, (bounds, Water::prepare(mesh)));
            }
        }

        for (bounds, mesh) in self.meshes.values() {
            if ctx.gfx.frustrum.intersects(bounds) {
                ctx.draw(mesh.clone().with_overview(self.overview.as_ref()));
            }
        }
    }

    fn reset(&mut self, env: &Environment) {
        self.meshes.clear();
        self.pending.clear();
        if env.has_water_configuration() {
            self.pending.extend(env.chunks().map(|(chunk, _)| chunk));
        }
    }
}

impl super::MapRenderer {
    pub(crate) fn set_water_overview(&mut self, overview: Option<Arc<WaterOverviewAtlas>>) {
        self.water.overview = overview;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Surface {
    kind: WaterKind,
    z: f32,
}

#[derive(Debug, PartialEq)]
struct SurfaceRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    surface: Surface,
}

fn for_each_rect(tiles: impl Iterator<Item = WaterTile>, mut emit: impl FnMut(SurfaceRect)) {
    let mut tiles = tiles.peekable();
    if tiles.peek().is_none() {
        return;
    }
    let mut cells = [None; RES * RES];
    for WaterTile { tile, sample } in tiles {
        cells[tile.local_index()] = Some(Surface {
            kind: sample.kind,
            z: sample.surface_z,
        });
    }
    for y in 0..RES {
        for x in 0..RES {
            let Some(surface) = cells[y * RES + x] else {
                continue;
            };
            let right = (x..RES)
                .find(|&col| cells[y * RES + col] != Some(surface))
                .unwrap_or(RES);
            let top = (y + 1..RES)
                .find(|&row| {
                    cells[row * RES + x..row * RES + right]
                        .iter()
                        .any(|cell| *cell != Some(surface))
                })
                .unwrap_or(RES);
            for row in y..top {
                cells[row * RES + x..row * RES + right].fill(None);
            }
            emit(SurfaceRect {
                x,
                y,
                width: right - x,
                height: top - y,
                surface,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulation::map::{WaterBodyId, WaterSample, WaterTileCoord};

    #[test]
    fn water_w4_mesh_merges_matching_tiles_and_preserves_holes_kind_and_height() {
        let tile = |x, y, kind, surface_z| WaterTile {
            tile: WaterTileCoord(x, y),
            sample: WaterSample {
                body: WaterBodyId(1),
                kind,
                surface_z,
                depth_m: 1.0,
                reach_m: None,
            },
        };
        let tiles = [
            tile(30, 0, WaterKind::River, 1.0),
            tile(31, 0, WaterKind::River, 1.0),
            tile(30, 1, WaterKind::River, 1.0),
            tile(31, 1, WaterKind::River, 1.0),
            tile(29, 1, WaterKind::Lake, 1.0),
            tile(31, 3, WaterKind::River, 2.0),
        ];
        let mut rects = Vec::new();
        for_each_rect(tiles.into_iter(), |rect| rects.push(rect));
        let coverage: BTreeMap<_, _> = rects
            .iter()
            .flat_map(|rect| {
                (rect.y..rect.y + rect.height).flat_map(move |y| {
                    (rect.x..rect.x + rect.width)
                        .map(move |x| ((x as i16, y as i16), (rect.surface.kind, rect.surface.z)))
                })
            })
            .collect();
        let expected: BTreeMap<_, _> = tiles
            .into_iter()
            .map(|tile| {
                (
                    (tile.tile.0, tile.tile.1),
                    (tile.sample.kind, tile.sample.surface_z),
                )
            })
            .collect();

        assert_eq!(coverage, expected);
        assert_eq!(rects.len(), 3);
    }
}

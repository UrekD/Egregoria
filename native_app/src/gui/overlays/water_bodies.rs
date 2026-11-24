use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use engine::{heightmap::GroundOverlayImage, GfxContext};
use geom::{Camera, InfiniteFrustrum, Intersect3, LinearColor, Vec2, Vec3, AABB, AABB3};
use goryak::{blur_bg, mincolumn, on_secondary_container, padxy, secondary_container, textc};
use simulation::map::{
    Environment, Heightmap, Map, MapSubscriber, TerrainChunkID, UpdateType, WaterKind,
};

use yakui::{reflow, Alignment, Dim2, Pivot};

use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::{water_color, MapRenderer, WATER_TILE_PITCH};

const POOL_WIDTH: usize = 4;
const POOLS_PER_CHUNK: usize = Heightmap::RESOLUTION / POOL_WIDTH;
const PITCH: f32 = WATER_TILE_PITCH * POOL_WIDTH as f32;
const ARROW_INTERVAL: f32 = 224.0;
const ARROW_COLOR: LinearColor = LinearColor {
    r: 0.78,
    g: 0.95,
    b: 1.0,
    a: 1.0,
};

thread_local! {
    static OVERVIEW: RefCell<Option<WaterOverview>> = const { RefCell::new(None) };
}

struct WaterOverview {
    world: u64,
    water_revision: u64,
    bounds: AABB,
    size: [u32; 2],
    rgba: Vec<u8>,
    wet_chunks: BTreeSet<TerrainChunkID>,
    changes: MapSubscriber,
    arrows: BTreeMap<TerrainChunkID, Vec<RiverArrow>>,
    uploaded: bool,
}

struct RiverArrow {
    point: Vec3,
    direction: Vec2,
}

pub(super) fn reset() {
    OVERVIEW.set(None);
}

pub(super) fn prepare(
    map: &Map,
    world: u64,
    mode_changed: bool,
    renderer: &mut MapRenderer,
    gfx: &mut GfxContext,
) {
    OVERVIEW.with_borrow_mut(|cached| {
        let env = map.environment();
        let revision = env.water_revision();
        if cached.as_ref().is_none_or(|c| c.world != world) {
            *cached = Some(WaterOverview::new(map, world));
        }
        let overview = cached.as_mut().unwrap();
        overview.refresh(env, revision);
        if overview.uploaded && !mode_changed {
            return;
        }
        let atlas = (!overview.wet_chunks.is_empty()).then_some(GroundOverlayImage {
            origin: overview.bounds.ll,
            extent: overview.bounds.ur - overview.bounds.ll,
            size: overview.size,
            rgba: &overview.rgba,
            // Keep revisions separate from the increasing GroundCache counter.
            revision: !revision,
        });
        overview.uploaded = renderer.set_ground_overlay(gfx, atlas, mode_changed);
    });
}

impl WaterOverview {
    fn new(map: &Map, world: u64) -> Self {
        let env = map.environment();
        let mut overview = Self {
            world,
            water_revision: env.water_revision(),
            bounds: AABB::zero(),
            size: [0; 2],
            rgba: Vec::new(),
            wet_chunks: BTreeSet::new(),
            changes: map.subscribe(UpdateType::Water),
            arrows: BTreeMap::new(),
            uploaded: false,
        };
        overview.rebuild(env);
        overview
    }

    fn refresh(&mut self, env: &Environment, revision: u64) {
        let cleared = self.changes.take_cleared();
        let mut dirty = self.changes.take_updated_chunks().peekable();
        if !cleared && self.water_revision == revision {
            return;
        }
        if cleared || dirty.peek().is_none() || !env.has_water_configuration() {
            self.rebuild(env);
        } else {
            for bucket in dirty {
                for chunk in bucket.convert() {
                    if env.get_chunk(chunk).is_some() {
                        self.update_chunk(env, chunk);
                    }
                }
            }
            self.arrows = river_arrows(env);
        }
        self.water_revision = revision;
        self.uploaded = false;
    }

    fn rebuild(&mut self, env: &Environment) {
        self.bounds = env.bounds();
        let extent = self.bounds.ur - self.bounds.ll;
        self.size = [
            (extent.x / PITCH).ceil() as u32,
            (extent.y / PITCH).ceil() as u32,
        ];
        self.rgba.clear();
        self.wet_chunks.clear();
        if env.has_water_configuration() {
            for (chunk, _) in env.chunks() {
                self.update_chunk(env, chunk);
            }
        }
        self.arrows = river_arrows(env);
    }

    fn update_chunk(&mut self, env: &Environment, chunk: TerrainChunkID) {
        let mut tile_pools = [[[0u8; 3]; POOLS_PER_CHUNK]; POOLS_PER_CHUNK];
        let mut wet = false;
        for tile in env.water_tiles(chunk) {
            let local = tile.tile.local_index();
            let x = (local % Heightmap::RESOLUTION) / POOL_WIDTH;
            let y = (local / Heightmap::RESOLUTION) / POOL_WIDTH;
            tile_pools[y][x][kind_index(tile.sample.kind)] += 1;
            wet = true;
        }
        if wet {
            self.wet_chunks.insert(chunk);
            self.rgba
                .resize((self.size[0] * self.size[1] * 4) as usize, 0);
        } else if !self.wet_chunks.remove(&chunk) {
            return;
        }

        let start = chunk.1 as usize * POOLS_PER_CHUNK * self.size[0] as usize
            + chunk.0 as usize * POOLS_PER_CHUNK;
        for (y, row) in tile_pools.into_iter().enumerate() {
            let offset = 4 * (start + y * self.size[0] as usize);
            for (coverage, pixel) in row
                .into_iter()
                .zip(self.rgba[offset..offset + 4 * POOLS_PER_CHUNK].chunks_exact_mut(4))
            {
                pixel.copy_from_slice(&pool_pixel(coverage));
            }
        }
    }
}

fn kind_index(kind: WaterKind) -> usize {
    match kind {
        WaterKind::Sea => 0,
        WaterKind::Lake => 1,
        WaterKind::River => 2,
    }
}

fn pool_pixel(coverage: [u8; 3]) -> [u8; 4] {
    // Keep a narrow river visible when its mouth shares a texel with the sea.
    for kind in [WaterKind::River, WaterKind::Lake, WaterKind::Sea] {
        let count = coverage[kind_index(kind)];
        if count > 0 {
            let color = water_color(kind);
            let opacity = (0.57 + 0.024 * f32::from(count)).min(0.95);
            return [color.r, color.g, color.b, 1.0].map(|c| (c * opacity * 255.0).round() as u8);
        }
    }
    [0; 4]
}

fn river_arrows(env: &Environment) -> BTreeMap<TerrainChunkID, Vec<RiverArrow>> {
    let mut arrows: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (id, kind, _) in env.freshwater_bodies() {
        if kind != WaterKind::River {
            continue;
        }
        let Some(line) = env.river_centerline(id) else {
            continue;
        };
        let mut remaining = ARROW_INTERVAL * 0.5;
        let mut any = false;
        let mut longest = (0.0, Vec2::ZERO, Vec2::ZERO);
        let mut push = |point: Vec2, direction: Vec2| {
            let Some(sample) = env.water_at(point).filter(|sample| sample.body == id) else {
                return false;
            };
            arrows
                .entry(TerrainChunkID::new(point))
                .or_default()
                .push(RiverArrow {
                    point: point.z(sample.surface_z + 0.35),
                    direction,
                });
            true
        };
        for segment in line.windows(2) {
            let delta = segment[1] - segment[0];
            let length = delta.mag();
            if length <= 0.0 {
                continue;
            }
            let direction = delta / length;
            if length > longest.0 {
                longest = (length, segment[0] + delta * 0.5, direction);
            }
            while remaining < length {
                any |= push(segment[0] + remaining * direction, direction);
                remaining += ARROW_INTERVAL;
            }
            remaining -= length;
        }
        if !any && longest.0 > 0.0 {
            push(longest.1, longest.2);
        }
    }
    arrows
}

pub(super) fn draw(camera: &Camera, draw: &mut ImmediateDraw) {
    OVERVIEW.with_borrow(|overview| {
        let Some(overview) = overview.as_ref() else {
            return;
        };
        let frustum =
            InfiniteFrustrum::from_reversez_invviewproj(camera.eye(), camera.inv_proj_cache);
        let length = (camera.dist * 0.045).clamp(12.0, 75.0);
        let width = (length * 0.10).max(1.0);
        for (chunk, arrows) in &overview.arrows {
            let xy = chunk.bbox().expand(length);
            let sample = arrows[0].point.z;
            let (low, high) = arrows.iter().fold((sample, sample), |(low, high), arrow| {
                (low.min(arrow.point.z), high.max(arrow.point.z))
            });
            if !frustum.intersects(&AABB3 {
                ll: xy.ll.z(low),
                ur: xy.ur.z(high),
            }) {
                continue;
            }
            for arrow in arrows {
                let tip = arrow.point + arrow.direction.z0() * (length * 0.5);
                let tail = tip - arrow.direction.z0() * length;
                let wing = arrow.direction.perpendicular().z0() * (length * 0.22);
                let head_base = tip - arrow.direction.z0() * (length * 0.34);
                draw.line(tail, tip, width).color(ARROW_COLOR);
                draw.line(head_base + wing, tip, width).color(ARROW_COLOR);
                draw.line(head_base - wing, tip, width).color(ARROW_COLOR);
            }
        }
    });
}

pub(super) fn legend() {
    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(12.0, 112.0),
        || {
            blur_bg(secondary_container().with_alpha(0.94), 8.0, || {
                padxy(10.0, 6.0, || {
                    mincolumn(4.0, || {
                        textc(on_secondary_container(), "Water bodies");
                        for (kind, label) in [
                            (WaterKind::Sea, "Sea"),
                            (WaterKind::River, "River"),
                            (WaterKind::Lake, "Lake"),
                        ] {
                            let color = water_color(kind);
                            super::legend::legend_item([color.r, color.g, color.b, 1.0], label);
                        }
                        super::legend::legend_item(
                            [ARROW_COLOR.r, ARROW_COLOR.g, ARROW_COLOR.b, 1.0],
                            "Downstream",
                        );
                        if OVERVIEW
                            .with_borrow(|o| o.as_ref().is_some_and(|o| o.wet_chunks.is_empty()))
                        {
                            textc(on_secondary_container(), "No water bodies");
                        }
                    });
                });
            });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec2;

    #[test]
    fn water_w4_overview_preserves_narrow_river_at_a_mouth() {
        assert_eq!(pool_pixel([0, 0, 0]), [0; 4]);
        assert_eq!(pool_pixel([14, 1, 1]), pool_pixel([0, 0, 1]));
        assert_ne!(pool_pixel([0, 0, 1]), pool_pixel([1, 0, 0]));
    }

    #[test]
    fn water_w4_arrows_use_persisted_direction() {
        let mut env = Environment::new(1, 1);
        env.carve_river_at_level(&[vec2(450.0, 240.0), vec2(50.0, 240.0)], 64.0, 4.0, Some(0))
            .unwrap();
        let arrows = river_arrows(&env);
        let points: Vec<_> = arrows.values().flatten().collect();
        assert_eq!(points.len(), 2);
        for arrow in points {
            assert_eq!(arrow.direction, -Vec2::X);
            assert_eq!(arrow.point.z, 0.35);
        }
    }
}

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use std::sync::Arc;

use engine::WaterOverviewAtlas;
use geom::{Camera, InfiniteFrustrum, Intersect3, LinearColor, Vec2, Vec3, AABB3};
use goryak::{blur_bg, mincolumn, on_secondary_container, padxy, secondary_container, textc};
use simulation::map::{
    Environment, Heightmap, Map, MapSubscriber, TerrainChunkID, UpdateType, WaterBodyId, WaterKind,
};

use yakui::{reflow, Alignment, Dim2, Pivot};

use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::{water_color, WATER_OVERVIEW_POOL};

const POOLS_PER_CHUNK: usize = Heightmap::RESOLUTION / WATER_OVERVIEW_POOL;
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
    atlas: Option<Arc<WaterOverviewAtlas>>,
    wet_chunks: BTreeMap<TerrainChunkID, BTreeSet<WaterBodyId>>,
    changes: MapSubscriber,
    arrows: BTreeMap<WaterBodyId, BTreeMap<TerrainChunkID, Vec<RiverArrow>>>,
}

struct RiverArrow {
    point: Vec3,
    direction: Vec2,
}

pub(super) fn reset() {
    OVERVIEW.set(None);
}

#[cfg(test)]
pub(super) fn world_cache_is_clear() -> bool {
    OVERVIEW.with_borrow(Option::is_none)
}

/// Snapshot only CPU-side coverage and arrow changes from this world revision.
pub(super) fn prepare(map: &Map, world: u64) -> Option<Arc<WaterOverviewAtlas>> {
    OVERVIEW.with_borrow_mut(|cached| {
        let overview = if let Some(overview) = cached.as_mut().filter(|c| c.world == world) {
            overview.refresh(map.environment());
            overview
        } else {
            cached.insert(WaterOverview::new(map, world))
        };
        overview.atlas.clone()
    })
}

impl WaterOverview {
    fn new(map: &Map, world: u64) -> Self {
        let env = map.environment();
        let mut overview = Self {
            world,
            water_revision: env.water_revision(),
            atlas: None,
            wet_chunks: BTreeMap::new(),
            changes: map.subscribe(UpdateType::Water),
            arrows: BTreeMap::new(),
        };
        overview.rebuild(env);
        overview
    }

    fn refresh(&mut self, env: &Environment) {
        let cleared = self.changes.take_cleared();
        let mut dirty = self.changes.take_updated_chunks().peekable();
        let revision = env.water_revision();
        if !cleared && self.water_revision == revision {
            return;
        }
        if cleared || dirty.peek().is_none() || !env.has_water_configuration() {
            self.rebuild(env);
        } else {
            let mut bodies = BTreeSet::new();
            for bucket in dirty {
                for chunk in bucket.convert() {
                    if env.get_chunk(chunk).is_some() {
                        self.update_chunk(env, chunk, &mut bodies);
                    }
                }
            }
            self.update_arrows(env, bodies);
        }
        self.water_revision = revision;
    }

    fn rebuild(&mut self, env: &Environment) {
        self.wet_chunks.clear();
        self.arrows.clear();
        self.atlas = None;
        if env.has_water_configuration() {
            let mut bodies = BTreeSet::new();
            for (chunk, _) in env.chunks() {
                self.update_chunk(env, chunk, &mut bodies);
            }
            self.update_arrows(env, bodies);
        }
    }

    fn update_arrows(&mut self, env: &Environment, bodies: BTreeSet<WaterBodyId>) {
        for id in bodies {
            if let Some(line) = env.river_centerline(id) {
                self.arrows.insert(id, river_arrows(env, id, line));
            } else {
                self.arrows.remove(&id);
            }
        }
    }

    fn update_chunk(
        &mut self,
        env: &Environment,
        chunk: TerrainChunkID,
        dirty_bodies: &mut BTreeSet<WaterBodyId>,
    ) {
        let mut tile_pools = [[[0u8; 3]; POOLS_PER_CHUNK]; POOLS_PER_CHUNK];
        let mut bodies = BTreeSet::new();
        for tile in env.water_tiles(chunk) {
            let local = tile.tile.local_index();
            let x = (local % Heightmap::RESOLUTION) / WATER_OVERVIEW_POOL;
            let y = (local / Heightmap::RESOLUTION) / WATER_OVERVIEW_POOL;
            tile_pools[y][x][kind_index(tile.sample.kind)] += 1;
            bodies.insert(tile.sample.body);
        }
        let previous = self.wet_chunks.remove(&chunk);
        if previous.is_none() && bodies.is_empty() {
            return;
        }
        if let Some(previous) = previous {
            dirty_bodies.extend(previous);
        }
        if !bodies.is_empty() {
            dirty_bodies.extend(&bodies);
            self.wet_chunks.insert(chunk, bodies);
        }

        let side = POOLS_PER_CHUNK as u32;
        let atlas = self.atlas.get_or_insert_with(|| {
            let (w, h) = env.size();
            Arc::new(WaterOverviewAtlas::new(
                [u32::from(w) * side, u32::from(h) * side],
                side,
            ))
        });
        let origin = [chunk.0 as u32 * side, chunk.1 as u32 * side];
        atlas.stage(
            origin,
            tile_pools
                .into_iter()
                .flatten()
                .flat_map(pool_pixel)
                .collect(),
        );
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
            return [color.r, color.g, color.b, opacity].map(|c| (c * 255.0).round() as u8);
        }
    }
    [0; 4]
}

fn river_arrows(
    env: &Environment,
    id: WaterBodyId,
    line: &[Vec2],
) -> BTreeMap<TerrainChunkID, Vec<RiverArrow>> {
    let mut arrows: BTreeMap<_, Vec<_>> = BTreeMap::new();
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
        for (chunk, arrows) in overview.arrows.values().flat_map(BTreeMap::iter) {
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
    fn water_w4_local_refresh_and_dry_replacement() {
        use simulation::map::{WaterEdit, WaterStroke};
        use simulation::Simulation;

        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let mut sim = Simulation::new_authored_flat(5, 0).unwrap();
        let river = |x| WaterEdit::CarveRiver {
            polyline: WaterStroke::new(&[vec2(x, 96.0), vec2(x, 450.0)]).unwrap(),
            width_m: 64.0,
            depth_m: 5.0,
            level_dm: Some(-10),
        };
        let near = sim.edit_water(river(200.0)).unwrap().body.unwrap();
        let far = sim.edit_water(river(1500.0)).unwrap().body.unwrap();
        let atlas = prepare(&sim.map(), 1).unwrap();
        let far_arrow = || {
            OVERVIEW.with_borrow(|cache| {
                cache.as_ref().unwrap().arrows[&far]
                    .values()
                    .next()
                    .unwrap()
                    .as_ptr()
            })
        };
        let existing_far = far_arrow();
        sim.edit_water(WaterEdit::RemoveFreshWater { body: near })
            .unwrap();
        assert!(Arc::ptr_eq(&atlas, &prepare(&sim.map(), 1).unwrap()));
        assert_eq!(existing_far, far_arrow(), "unrelated river arrows rebuilt");
        assert!(OVERVIEW.with_borrow(|cache| !cache.as_ref().unwrap().arrows.contains_key(&near)));

        let dry = Simulation::new_authored_flat(5, 0).unwrap();
        assert!(prepare(&dry.map(), 2).is_none());
        assert!(OVERVIEW.with_borrow(|cache| cache.as_ref().unwrap().arrows.is_empty()));
        reset();
        assert!(world_cache_is_clear());
    }

    #[test]
    fn water_w4_arrows_use_persisted_direction() {
        let mut env = Environment::new(1, 1);
        env.carve_river_at_level(&[vec2(450.0, 240.0), vec2(50.0, 240.0)], 64.0, 4.0, Some(0))
            .unwrap();
        let (id, _, _) = env.freshwater_bodies().next().unwrap();
        let arrows = river_arrows(&env, id, env.river_centerline(id).unwrap());
        let points: Vec<_> = arrows.values().flatten().collect();
        assert_eq!(points.len(), 2);
        for arrow in points {
            assert_eq!(arrow.direction, -Vec2::X);
            assert_eq!(arrow.point.z, 0.35);
        }
    }
}

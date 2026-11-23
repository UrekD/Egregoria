use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use engine::heightmap::{GroundOverlayDisc, GroundOverlayImage};
use engine::GfxContext;
use geom::{vec2, LinearColor, Vec2};
use prototypes::{try_prototype, ServiceID};
use simulation::map::{ElectricityNetworkID, Map, MapSubscriber, UpdateType};
use simulation::map_dynamic::{
    service_reach_sources, utility_network_at_road, utility_network_readout, ElectricityFlow,
    EnvGrid, ServiceReachSource, UtilityFlow, UtilityNetworkReadout, POLLUTION_RADIUS,
};
use simulation::souls::happiness::Happiness;
use simulation::Simulation;

use super::{current_overlay, legend, placement_overlay, OverlayMode};
use crate::gui::{PlacementPreview, Tool};
use crate::rendering::MapRenderer;
use crate::uiworld::UiWorld;

const PITCH: f32 = EnvGrid::CELL_SIZE;
const FIELD_INTERVAL: Duration = Duration::from_millis(500);
const SCORE_RADIUS: f32 = 128.0;

#[derive(Default)]
struct GroundCache {
    world: Option<u64>,
    changes: Option<MapSubscriber>,
    mode: OverlayMode,
    key: Option<FieldKey>,
    last_upload: Option<Instant>,
    revision: u64,
    empty: bool,
    placement_network: Option<UtilityNetworkReadout>,
}

#[derive(PartialEq, Eq)]
struct FieldKey {
    topology: u64,
    data: u64,
    focus: Option<ElectricityNetworkID>,
}

thread_local! {
    static CACHE: RefCell<GroundCache> = RefCell::new(GroundCache::default());
}

pub(super) fn reset_world_cache() {
    CACHE.with_borrow_mut(|cache| *cache = GroundCache::default());
}

#[cfg(test)]
pub(super) fn world_cache_is_clear() -> bool {
    CACHE.with_borrow(|cache| cache.world.is_none() && cache.key.is_none())
}

pub(super) fn is_empty(mode: OverlayMode) -> bool {
    CACHE.with_borrow(|cache| cache.mode == mode && cache.empty)
}

pub(super) fn placement_network() -> Option<UtilityNetworkReadout> {
    CACHE.with_borrow(|cache| cache.placement_network.clone())
}

pub(crate) fn prepare_ground(
    uiw: &UiWorld,
    sim: &Simulation,
    renderer: &mut MapRenderer,
    gfx: &mut GfxContext,
    world_generation: u64,
) {
    let mode = current_overlay(*uiw.read::<OverlayMode>(), uiw);
    let preview = (*uiw.read::<Tool>() == Tool::SpecialBuilding && !crate::session::modal(uiw))
        .then(|| uiw.read::<PlacementPreview>());
    let preview = preview.as_deref();
    renderer.set_overlay_preview(gfx, preview.and_then(preview_disc));
    let placement_network = preview
        .filter(|_| mode.is_utility())
        .and_then(|preview| preview.ghost.map(|(_, road)| road))
        .and_then(|road| utility_network_at_road(sim, road));
    let focus = placement_network
        .as_ref()
        .and_then(|network| network.network);

    CACHE.with_borrow_mut(|cache| {
        let new_world = cache.world != Some(world_generation);
        let mode_changed = cache.mode != mode || new_world;
        if new_world {
            *cache = GroundCache {
                world: Some(world_generation),
                ..GroundCache::default()
            };
        }
        cache.placement_network = placement_network;
        let Some(scale) = mode.scale() else {
            if mode_changed {
                renderer.set_ground_overlay(gfx, None, true);
            }
            cache.mode = mode;
            cache.key = None;
            cache.last_upload = None;
            return;
        };

        let map = sim.map();
        let changes = cache.changes.get_or_insert_with(|| {
            map.subscribe(UpdateType::Building | UpdateType::Road | UpdateType::Terrain)
        });
        let topology_changed =
            changes.take_cleared() | changes.take_updated_chunks().next().is_some();
        // Keep the key dirty when a change arrives during the upload cooldown.
        if topology_changed {
            cache.key = None;
        }
        let key = FieldKey {
            topology: UtilityFlow::topology_revision(&map),
            data: mode.data_version(sim, &map),
            focus,
        };
        if !mode_changed
            && (cache.key.as_ref() == Some(&key)
                || cache
                    .last_upload
                    .is_some_and(|at| at.elapsed() < FIELD_INTERVAL))
        {
            return;
        }
        if new_world {
            renderer.set_ground_overlay(gfx, None, true);
        }

        let bounds = map.environment.bounds();
        let mut atlas = Atlas::new(bounds.ll, bounds.ur - bounds.ll);
        cache.mode = mode;
        if atlas.len() == 0 {
            renderer.set_ground_overlay(gfx, None, true);
            cache.key = Some(key);
            cache.empty = true;
            return;
        }

        match mode {
            OverlayMode::Coverage(kind) => {
                for (pixel, reach) in atlas.rgba.chunks_exact_mut(4).zip(radial_field(
                    &atlas.layout,
                    &service_reach_sources(sim, kind),
                )) {
                    encode_reach(pixel, reach);
                }
            }
            OverlayMode::Pollution => {
                for (pixel, pollution) in atlas
                    .rgba
                    .chunks_exact_mut(4)
                    .zip(pollution_field(&atlas.layout, &sim.read::<EnvGrid>()))
                {
                    encode(
                        pixel,
                        scale.color(pollution),
                        (pollution / 18.0).sqrt().min(1.0),
                    );
                }
            }
            OverlayMode::LandValue | OverlayMode::Happiness => {
                let field = if mode == OverlayMode::LandValue {
                    score_field(
                        &atlas.layout,
                        land_value_samples(&map, &sim.read::<EnvGrid>()),
                    )
                } else {
                    let happiness = sim.read::<Happiness>();
                    let scores = map.buildings().values().filter_map(|building| {
                        let score = happiness.by_building.get(&building.id)?;
                        Some((building.obb.center(), f32::from(*score)))
                    });
                    score_field(&atlas.layout, scores)
                };
                for (pixel, [sum, weight]) in atlas.rgba.chunks_exact_mut(4).zip(field) {
                    if weight > 0.0 {
                        encode(pixel, scale.color(sum / weight), weight.min(1.0));
                    }
                }
            }
            OverlayMode::Power | OverlayMode::Water | OverlayMode::Sewage => {
                atlas.paint_roads(sim, &map, mode, focus);
            }
            OverlayMode::Off | OverlayMode::Traffic | OverlayMode::Transit => unreachable!(),
        }
        cache.revision = cache.revision.wrapping_add(1);
        if renderer.set_ground_overlay(gfx, Some(atlas.image(cache.revision)), mode_changed) {
            cache.key = Some(key);
            cache.last_upload = Some(Instant::now());
            cache.empty = atlas.rgba.chunks_exact(4).all(|pixel| pixel[3] == 0);
        }
    });
}

pub(super) fn preview_disc(preview: &PlacementPreview) -> Option<GroundOverlayDisc> {
    let company = preview.company?;
    let (obb, _) = preview.ghost?;
    let (radius, color) = match placement_overlay(company)? {
        OverlayMode::Coverage(_) => {
            let [r, g, b, a] = legend::MAX_REACH_COLOR;
            (
                try_prototype(ServiceID::from(company))?.radius_m,
                LinearColor::new(r, g, b, a),
            )
        }
        OverlayMode::Pollution => (POLLUTION_RADIUS, LinearColor::new(0.95, 0.28, 0.08, 0.5)),
        _ => return None,
    };
    Some(GroundOverlayDisc {
        center: obb.center(),
        radius,
        color,
    })
}

struct Atlas {
    layout: AtlasLayout,
    rgba: Vec<u8>,
}

struct AtlasLayout {
    origin: Vec2,
    extent: Vec2,
    size: [u32; 2],
}

impl AtlasLayout {
    fn len(&self) -> usize {
        self.size[0] as usize * self.size[1] as usize
    }

    fn center(&self, x: i32, y: i32) -> Vec2 {
        self.origin + vec2(x as f32 + 0.5, y as f32 + 0.5) * PITCH
    }

    fn visit(&self, min: Vec2, max: Vec2, mut sample: impl FnMut(usize, Vec2)) {
        let to_texel = |pos: Vec2| (pos - self.origin) / PITCH - Vec2::splat(0.5);
        let min = to_texel(min);
        let max = to_texel(max);
        let x0 = (min.x.ceil() as i32).max(0);
        let y0 = (min.y.ceil() as i32).max(0);
        let x1 = (max.x.floor() as i32).min(self.size[0] as i32 - 1);
        let y1 = (max.y.floor() as i32).min(self.size[1] as i32 - 1);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let index = y as usize * self.size[0] as usize + x as usize;
                sample(index, self.center(x, y));
            }
        }
    }
}

impl Atlas {
    fn new(origin: Vec2, extent: Vec2) -> Self {
        let size = [
            (extent.x / PITCH).ceil() as u32,
            (extent.y / PITCH).ceil() as u32,
        ];
        let layout = AtlasLayout {
            origin,
            extent,
            size,
        };
        let rgba = vec![0; layout.len() * 4];
        Self { layout, rgba }
    }

    fn len(&self) -> usize {
        self.layout.len()
    }

    fn image(&self, revision: u64) -> GroundOverlayImage<'_> {
        GroundOverlayImage {
            origin: self.layout.origin,
            extent: self.layout.extent,
            size: self.layout.size,
            rgba: &self.rgba,
            revision,
        }
    }

    fn paint_roads(
        &mut self,
        sim: &Simulation,
        map: &Map,
        mode: OverlayMode,
        focus: Option<ElectricityNetworkID>,
    ) {
        let power = sim.read::<ElectricityFlow>();
        let flow = sim.read::<UtilityFlow>();
        let mut colors = BTreeMap::new();
        for (id, road) in map.roads() {
            let network = map.electricity.net_id(id);
            let color = *colors.entry(network).or_insert_with(|| {
                let readout = utility_network_readout(map, &power, &flow, network);
                legend::utility_color(mode.utility_state(&readout))
            });
            let focused = focus.is_some() && focus == network;
            // Extend onto the shoulders so the tint is visible beside the road mesh.
            let radius = road.width * 0.5 + if focused { 52.0 } else { 36.0 };
            let opacity = match focus {
                Some(_) if focused => 1.0,
                Some(_) => 0.34,
                None => 0.75,
            };
            for segment in road.points().as_slice().windows(2) {
                let [start, end] = [segment[0].xy(), segment[1].xy()];
                let span = end - start;
                let length_squared = span.mag2().max(f32::EPSILON);
                let margin = Vec2::splat(radius);
                self.layout.visit(
                    start.min(end) - margin,
                    start.max(end) + margin,
                    |i, pos| {
                        let along = ((pos - start).dot(span) / length_squared).clamp(0.0, 1.0);
                        let distance = pos.distance(start + along * span);
                        let fade = ((radius - distance) / PITCH).clamp(0.0, 1.0);
                        let pixel = &mut self.rgba[i * 4..i * 4 + 4];
                        if alpha_byte(color[3] * opacity * fade) > pixel[3] {
                            encode(pixel, color, opacity * fade);
                        }
                    },
                );
            }
        }
    }
}

fn alpha_byte(alpha: f32) -> u8 {
    (255.0 * alpha.clamp(0.0, 1.0)).round() as u8
}

fn encode(out: &mut [u8], [r, g, b, opacity]: [f32; 4], weight: f32) {
    let alpha = (opacity * weight).clamp(0.0, 1.0);
    out.copy_from_slice(&[
        alpha_byte(r * alpha),
        alpha_byte(g * alpha),
        alpha_byte(b * alpha),
        alpha_byte(alpha),
    ]);
}

fn encode_reach(pixel: &mut [u8], reach: f32) {
    encode(pixel, legend::REACH_COLOR, reach);
}

fn radial_field(layout: &AtlasLayout, sources: &[ServiceReachSource]) -> Vec<f32> {
    let mut field = vec![0.0_f32; layout.len()];
    for source in sources {
        if source.effective_capacity <= 0.0 || source.served_distance_m <= 0.0 {
            continue;
        }
        let (center, radius) = (source.center, source.served_distance_m);
        let margin = Vec2::splat(radius);
        layout.visit(center - margin, center + margin, |i, pos| {
            field[i] = field[i].max(1.0 - pos.distance(center) / radius);
        });
    }
    field
}

fn pollution_field(layout: &AtlasLayout, grid: &EnvGrid) -> Vec<f32> {
    let mut field = vec![0.0; layout.len()];
    let first_cell = layout.origin / PITCH;
    for (&(x, y), cell) in &grid.cells {
        let x = x - first_cell.x as i32;
        let y = y - first_cell.y as i32;
        if (0..layout.size[0] as i32).contains(&x) && (0..layout.size[1] as i32).contains(&y) {
            field[y as usize * layout.size[0] as usize + x as usize] = cell.pollution;
        }
    }
    field
}

fn land_value_samples<'a>(
    map: &'a Map,
    env: &'a EnvGrid,
) -> impl Iterator<Item = (Vec2, f32)> + 'a {
    map.buildings().values().filter_map(move |building| {
        let score = env.land_value_sample(building.id)?;
        Some((building.obb.center(), f32::from(score)))
    })
}

fn score_field(layout: &AtlasLayout, scores: impl Iterator<Item = (Vec2, f32)>) -> Vec<[f32; 2]> {
    let mut field = vec![[0.0; 2]; layout.len()];
    for (center, score) in scores {
        let margin = Vec2::splat(SCORE_RADIUS);
        layout.visit(center - margin, center + margin, |i, pos| {
            let weight = (1.0 - pos.distance(center) / SCORE_RADIUS).max(0.0).powi(2);
            field[i][0] += weight * score;
            field[i][1] += weight;
        });
    }
    field
}
#[cfg(test)]
mod tests {
    use geom::{vec2, Vec2};
    use simulation::economy::balance::DEMAND_INTERVAL_TICKS;
    use simulation::map_dynamic::{EnvCell, EnvGrid, ServiceReachSource};

    use super::*;

    #[test]
    fn u3_radial_falloff_overlap_and_inactive_source() {
        let atlas = Atlas::new(Vec2::ZERO, Vec2::splat(160.0));
        let left = ServiceReachSource {
            building: Default::default(),
            center: vec2(16.0, 16.0),
            served_distance_m: 128.0,
            effective_capacity: 1.0,
        };
        let right = ServiceReachSource {
            center: vec2(112.0, 16.0),
            ..left
        };
        let inactive = ServiceReachSource {
            center: vec2(144.0, 144.0),
            effective_capacity: 0.0,
            ..left
        };
        assert!(radial_field(&atlas.layout, &[])
            .iter()
            .all(|&value| value == 0.0));
        assert!(radial_field(&atlas.layout, &[inactive])
            .iter()
            .all(|&value| value == 0.0));
        let field = radial_field(&atlas.layout, &[left, right, inactive]);
        assert_eq!(&field[..5], &[1.0, 0.75, 0.75, 1.0, 0.75]);
        assert_eq!(field[5], 0.75);
        assert_eq!(field[24], 0.0);
    }

    #[test]
    fn u3_coverage_field_has_linear_alpha() {
        let mut pixel = [0; 4];
        encode_reach(&mut pixel, 0.5);
        assert_eq!(pixel[3], alpha_byte(0.5 * legend::REACH_COLOR[3]));
    }

    #[test]
    fn u3_land_value_uses_measured_zero_and_skips_a_new_building() {
        use geom::OBB;
        use prototypes::{BuildingGen, GameTime, Tick};
        use scenarios::ScenarioCtx;
        use simulation::map::BuildingKind;
        use simulation::map_dynamic::env_update;
        use simulation::SimulationOptions;

        let mut fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            ..SimulationOptions::LEGACY
        });
        let build = |sim: &Simulation, center| {
            sim.write::<Map>()
                .build_special_building(
                    &OBB::new(center, Vec2::X, 20.0, 20.0),
                    BuildingKind::House,
                    BuildingGen::House,
                    None,
                    None,
                )
                .unwrap();
        };
        let measured = vec2(720.0, 720.0);
        build(&fixture.sim, measured);
        fixture
            .sim
            .write::<EnvGrid>()
            .cells
            .insert((22, 22), EnvCell { pollution: 200.0 });
        *fixture.sim.write::<GameTime>() = GameTime::new(Tick(DEMAND_INTERVAL_TICKS + 2));
        env_update(&mut fixture.sim);
        build(&fixture.sim, vec2(784.0, 720.0));

        let samples: Vec<_> =
            land_value_samples(&fixture.sim.map(), &fixture.sim.read::<EnvGrid>()).collect();
        assert_eq!(samples, [(measured, 0.0)]);
    }

    #[test]
    fn u3_scores_interpolate_only_within_reach_of_samples() {
        let atlas = Atlas::new(Vec2::ZERO, vec2(320.0, 32.0));
        let field = score_field(
            &atlas.layout,
            [(vec2(16.0, 16.0), 20.0), (vec2(80.0, 16.0), 80.0)].into_iter(),
        );
        let [sum, weight] = field[1];
        assert_eq!(sum / weight, 50.0);
        assert_eq!(field[9], [0.0, 0.0]);
    }

    #[test]
    fn u3_pollution_matches_cell_centres_and_leaves_gaps_clear() {
        let atlas = Atlas::new(vec2(32.0, 64.0), vec2(96.0, 32.0));
        let mut grid = EnvGrid::default();
        grid.cells.insert((1, 2), EnvCell { pollution: 2.5 });
        grid.cells.insert((2, 2), EnvCell { pollution: 71.0 });
        grid.cells.insert((4, 2), EnvCell { pollution: 30.0 });
        assert_eq!(pollution_field(&atlas.layout, &grid), vec![2.5, 71.0, 0.0]);
    }
}

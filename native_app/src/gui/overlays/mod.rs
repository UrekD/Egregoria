mod legend;
mod traffic;
mod transit;
pub(crate) mod view;

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};

use common::{ChunkID_256, FastMap};
use geom::{vec2, InfiniteFrustrum, Intersect3, Shape, Vec2, AABB, AABB3, OBB};
use goryak::{combo_box, tooltip};
use prototypes::{try_prototype, ServiceID, ServiceKind, TICKS_PER_HOUR};
use serde::{Deserialize, Deserializer};
use simulation::map::{Building, BuildingID, BuildingKind, Map, MapSubscriber, UpdateType};
use simulation::map_dynamic::{
    CoverageGrid, ElectricityFlow, EnvGrid, Transit, UtilityFlow, UtilityStatus,
};
use simulation::souls::happiness::Happiness;
use simulation::souls::residence::{self, Residences};
use simulation::Simulation;

use crate::gui::hud::toolbox::building::draw_service_radius;
use crate::rendering::immediate::ImmediateDraw;

use crate::uiworld::UiWorld;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) enum OverlayMode {
    #[default]
    Off,
    Power,
    Water,
    Sewage,
    Coverage(#[serde(deserialize_with = "deserialize_coverage")] ServiceKind),
    Pollution,
    LandValue,
    Happiness,
    Transit,
    Traffic,
}

fn deserialize_coverage<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<ServiceKind, D::Error> {
    let kind = ServiceKind::deserialize(deserializer)?;
    match kind {
        ServiceKind::WaterPump | ServiceKind::SewageOutlet => Err(serde::de::Error::custom(
            "utility services have no coverage overlay",
        )),
        _ => Ok(kind),
    }
}

#[derive(Default)]
struct OverlayCache {
    mode: OverlayMode,
    map: usize,
    changes: Option<MapSubscriber>,
    tick: u64,
    data_version: u64,
    projection: [f32; 16],
    tiles: TileIndex,
}

#[derive(Default)]
struct TileIndex {
    chunks: FastMap<ChunkID_256, Vec<Tile>>,
    padding: f32,
}

struct Tile {
    footprint: OBB,
    bounds: AABB3,
    color: [f32; 4],
}

impl TileIndex {
    fn clear(&mut self) {
        self.chunks.clear();
        self.padding = 0.0;
    }

    fn push(&mut self, footprint: OBB, z: f32, color: [f32; 4]) {
        let bounds = footprint.bbox();
        self.padding = self.padding.max(bounds.w().max(bounds.h()) * 0.5);
        self.chunks
            .entry(ChunkID_256::new(footprint.center()))
            .or_default()
            .push(Tile {
                footprint,
                bounds: AABB3::new(bounds.ll.z(z), bounds.ur.z(z)),
                color,
            });
    }

    fn push_outline(&mut self, footprint: OBB, z: f32, color: [f32; 4]) {
        const WIDTH: f32 = 5.0;
        let corners = footprint.corners;
        for (start, end) in corners.into_iter().zip(corners.into_iter().cycle().skip(1)) {
            let edge = end - start;
            let strip = OBB::new(
                (start + end) * 0.5,
                edge.normalize(),
                edge.mag() + WIDTH,
                WIDTH,
            );
            self.push(strip, z, color);
        }
    }

    fn visible_chunks(&self, bounds: AABB) -> impl Iterator<Item = &Tile> {
        let bounds = bounds.expand(self.padding);
        let ll = ChunkID_256::new(bounds.ll);
        let ur = ChunkID_256::new(bounds.ur);
        (ll.1..=ur.1)
            .flat_map(move |y| (ll.0..=ur.0).map(move |x| ChunkID_256::new_i16(x, y)))
            .filter_map(|chunk| self.chunks.get(&chunk))
            .flatten()
    }
}

thread_local! {
    static CACHE: RefCell<OverlayCache> = RefCell::new(OverlayCache::default());
}

pub(crate) fn reset_world_cache() {
    CACHE.with_borrow_mut(|cache| *cache = OverlayCache::default());
    view::reset_world_cache();
}

#[cfg(test)]
pub(crate) fn world_cache_is_clear() -> bool {
    CACHE.with_borrow(|cache| cache.changes.is_none() && cache.tiles.chunks.is_empty())
        && view::world_cache_is_clear()
}

const FOOTPRINT_PADDING: f32 = 6.0;

impl OverlayMode {
    fn data_version(self, sim: &Simulation, map: &Map, tick: u64) -> u64 {
        match self {
            Self::Off | Self::Traffic => 0,
            Self::Power => power_version(map, &sim.read::<ElectricityFlow>()),
            Self::Transit => transit::invalidation_key(&sim.read::<Transit>()),
            Self::Coverage(kind) => sim.read::<CoverageGrid>().revision(kind),
            Self::Water | Self::Sewage | Self::Pollution | Self::LandValue => tick / 3000,
            Self::Happiness => tick / TICKS_PER_HOUR,
        }
    }
}

fn power_version(map: &Map, flow: &ElectricityFlow) -> u64 {
    let mut hash = DefaultHasher::new();
    for network in map.electricity.networks() {
        network.id.hash(&mut hash);
        flow.blackout(network.id).hash(&mut hash);
    }
    hash.finish()
}

pub(crate) fn draw(sim: &Simulation, uiworld: &UiWorld) {
    let mode = *uiworld.read::<OverlayMode>();
    let mut draw = uiworld.write::<ImmediateDraw>();
    draw_service_radius(sim, uiworld, &mut draw);

    CACHE.with_borrow_mut(|cache| {
        if mode == OverlayMode::Off {
            if cache.mode != mode {
                draw.clear_persistent();
                cache.tiles.clear();
                cache.mode = mode;
            }
            return;
        }

        let map = sim.map();
        let map_id = std::ptr::from_ref(&map.subscribers) as usize;
        let new_map = cache.map != map_id || cache.changes.is_none();
        if new_map {
            cache.map = map_id;
            cache.changes = Some(
                map.subscribers
                    .subscribe(UpdateType::Building | UpdateType::Road | UpdateType::Terrain),
            );
        }
        let changes = cache.changes.as_mut().unwrap();
        let cleared = changes.take_cleared();
        // Building/road notifications also invalidate the power topology.
        let map_changed = changes.take_updated_chunks().next().is_some() || cleared || new_map;

        let tick = sim.get_tick();
        let data_version = mode.data_version(sim, &map, tick);
        let rebuild = mode != cache.mode
            || map_changed
            || tick < cache.tick
            || data_version != cache.data_version;
        cache.tick = tick;
        if rebuild {
            cache.mode = mode;
            cache.data_version = data_version;
            collect_tiles(sim, &map, mode, &mut cache.tiles);
        }

        match mode {
            OverlayMode::Transit => {
                if rebuild {
                    draw.clear_persistent();
                    transit::draw(&map, &sim.read::<Transit>(), &mut draw);
                }
                return;
            }
            OverlayMode::Traffic => {
                if rebuild {
                    draw.clear_persistent();
                }
                traffic::draw(sim, &map, uiworld, &mut draw, rebuild);
                return;
            }
            _ => {}
        }

        let camera = uiworld.camera().camera;
        let projection = *camera.proj_cache.as_ref();
        if rebuild || cache.projection != projection {
            cache.projection = projection;
            draw.clear_persistent();
            if let Some(bounds) = view::bounds(&camera, &map) {
                let frustum = InfiniteFrustrum::from_reversez_invviewproj(
                    camera.eye(),
                    camera.inv_proj_cache,
                );
                for tile in cache.tiles.visible_chunks(bounds) {
                    if frustum.intersects(&tile.bounds) {
                        draw.obb(tile.footprint, tile.bounds.ll.z)
                            .color(tile.color)
                            .persistent();
                    }
                }
            }
        }
    });
}

fn collect_tiles(sim: &Simulation, map: &Map, mode: OverlayMode, tiles: &mut TileIndex) {
    profiling::scope!("overlays::collect_tiles");
    tiles.clear();
    let Some(scale) = mode.scale() else {
        return;
    };

    if mode == OverlayMode::Pollution {
        let env = sim.read::<EnvGrid>();
        let size = EnvGrid::CELL_SIZE;
        for (&(x, y), cell) in &env.cells {
            let center = vec2(x as f32 + 0.5, y as f32 + 0.5) * size;
            if let Some(z) = map.environment.height(center) {
                tiles.push(
                    OBB::new(center, Vec2::X, size, size),
                    z + 0.2,
                    scale.color(cell.pollution),
                );
            }
        }
        return;
    }

    if let OverlayMode::Coverage(kind) = mode {
        let coverage = sim.read::<CoverageGrid>();
        for building in map.buildings().values() {
            let service = building
                .kind
                .as_goods_company()
                .and_then(|id| try_prototype(ServiceID::from(id)));
            let z = building.height + 0.35;
            if service.is_some_and(|proto| proto.service_kind == kind) {
                tiles.push_outline(
                    building.obb.expand(2.0 * FOOTPRINT_PADDING),
                    z,
                    legend::COVERAGE_PROVIDER_COLOR,
                );
            } else if is_coverage_receiver(building.kind) {
                tiles.push(
                    building.obb.expand(FOOTPRINT_PADDING),
                    z,
                    scale.color(coverage.coverage(building.id, kind)),
                );
            }
        }
        return;
    }

    let mut buildings = |value: &dyn Fn(&Building) -> Option<f32>| {
        for building in map.buildings().values() {
            if let Some(value) = value(building) {
                // Keep a visible border around asset meshes and procedural boxes.
                let footprint = match building.kind {
                    BuildingKind::GoodsCompany(company)
                        if company.is_growable()
                            || try_prototype(company).is_none_or(|proto| {
                                proto.asset.is_mesh() || proto.zone.is_some()
                            }) =>
                    {
                        building.obb.expand(FOOTPRINT_PADDING)
                    }
                    _ => building.obb,
                };
                tiles.push(footprint, building.height + 0.35, scale.color(value));
            }
        }
    };

    match mode {
        OverlayMode::Power => {
            let flow = sim.read::<ElectricityFlow>();
            buildings(&|b| {
                matches!(b.kind, BuildingKind::House | BuildingKind::GoodsCompany(_))
                    .then(|| f32::from(power_available(map, &flow, b.id)))
            });
        }
        OverlayMode::Water | OverlayMode::Sewage => {
            let flow = sim.read::<UtilityFlow>();
            let residences = sim.read::<Residences>();
            buildings(&|b| {
                if b.kind == BuildingKind::House && !residence::uses_water(sim, b.id) {
                    return None;
                }
                let status = flow.status(map, &residences, b.id)?;
                let relevant = if mode == OverlayMode::Water {
                    status.water
                } else {
                    status.sewage
                };
                Some(f32::from(relevant == UtilityStatus::Served))
            });
        }
        OverlayMode::LandValue => {
            let env = sim.read::<EnvGrid>();
            buildings(&|b| is_coverage_receiver(b.kind).then(|| f32::from(env.land_value(b.id))));
        }
        OverlayMode::Happiness => {
            let happiness = sim.read::<Happiness>();
            buildings(&|b| happiness.by_building.get(&b.id).copied().map(f32::from));
        }
        OverlayMode::Coverage(_)
        | OverlayMode::Pollution
        | OverlayMode::Off
        | OverlayMode::Transit
        | OverlayMode::Traffic => {}
    }
}

pub(crate) fn is_coverage_receiver(kind: BuildingKind) -> bool {
    matches!(kind, BuildingKind::House)
        || kind.as_goods_company().is_some_and(|id| id.is_growable())
}

pub(crate) fn power_available(map: &Map, flow: &ElectricityFlow, building: BuildingID) -> bool {
    map.get(building).is_some_and(|b| !b.is_orphaned())
        && map
            .electricity
            .net_id(building)
            .is_some_and(|network| !flow.blackout(network))
}

pub(crate) fn selector(uiworld: &UiWorld, show_unfinished_ui: bool) {
    use OverlayMode::*;

    const MODES: [(OverlayMode, &str); 15] = [
        (Off, "Overlay off"),
        (Power, "Power"),
        (Water, "Water"),
        (Sewage, "Sewage"),
        (Coverage(ServiceKind::Health), "Health coverage"),
        (Coverage(ServiceKind::Fire), "Fire coverage"),
        (Coverage(ServiceKind::Police), "Police coverage"),
        (Coverage(ServiceKind::School), "School coverage"),
        (Coverage(ServiceKind::Garbage), "Garbage coverage"),
        (Coverage(ServiceKind::Park), "Park coverage"),
        (Pollution, "Pollution"),
        (LandValue, "Land value"),
        (Happiness, "Happiness"),
        (Transit, "Transit"),
        (Traffic, "Traffic"),
    ];

    // Traffic is the last entry until its renderer and legend land.
    let show_traffic = traffic::AVAILABLE || show_unfinished_ui;
    let count = MODES.len() - usize::from(!show_traffic);
    let modes = &MODES[..count];
    let labels = MODES.map(|(_, label)| label);
    let mut mode = uiworld.write::<OverlayMode>();
    let mut selected = modes
        .iter()
        .position(|(option, _)| *option == *mode)
        .unwrap_or(0);
    if tooltip(labels[selected], || {
        combo_box(&mut selected, &labels[..count], 170.0)
    }) {
        *mode = modes[selected].0;
    }
    let empty = CACHE.with_borrow(|cache| cache.mode == *mode && cache.tiles.chunks.is_empty());
    match *mode {
        Transit => {
            let empty = uiworld.read::<ImmediateDraw>().persistent_orders.is_empty();
            transit::legend(empty);
        }
        Traffic if !show_traffic => {}
        _ => legend::render(*mode, labels[selected], empty, uiworld),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_overlay_uses_high_opacity_for_uncovered_receivers() {
        let scale = OverlayMode::Coverage(ServiceKind::School).scale().unwrap();
        let [r, g, b, alpha] = scale.color(0.0);
        assert!(r >= 0.5 && g <= 0.025 && b <= 0.025);
        assert!(alpha >= 0.9);
    }

    #[test]
    fn coverage_overlay_draws_receivers_and_only_the_selected_service_kind() {
        use prototypes::{BuildingGen, GoodsCompanyID};
        use scenarios::ScenarioCtx;
        use simulation::SimulationOptions;

        let test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let sites = [
            BuildingKind::House,
            BuildingKind::House,
            BuildingKind::GoodsCompany(GoodsCompanyID::new("school")),
            BuildingKind::GoodsCompany(GoodsCompanyID::new("clinic")),
        ];
        let mut map = test.sim.write::<Map>();
        let buildings = sites.map(|kind| {
            let center = vec2(400.0 + 40.0 * map.buildings().len() as f32, 450.0);
            let id = map
                .build_special_building(
                    &OBB::new(center, Vec2::X, 20.0, 20.0),
                    kind,
                    BuildingGen::CenteredDoor {
                        vertical_factor: 1.0,
                    },
                    None,
                    None,
                )
                .unwrap();
            (id, center)
        });
        drop(map);
        test.sim
            .write::<CoverageGrid>()
            .by_kind
            .entry(ServiceKind::School)
            .or_default()
            .insert(buildings[0].0, 95.0);

        let mut tiles = TileIndex::default();
        let mode = OverlayMode::Coverage(ServiceKind::School);
        let scale = mode.scale().unwrap();
        collect_tiles(&test.sim, &test.sim.map(), mode, &mut tiles);
        let color_at = |index: usize| {
            tiles
                .chunks
                .values()
                .flatten()
                .find(|tile| tile.footprint.center() == buildings[index].1)
                .map(|tile| tile.color)
        };
        assert_eq!(color_at(0), Some(scale.color(95.0)));
        assert_eq!(color_at(1), Some(scale.color(0.0)));
        let outlines: Vec<_> = tiles
            .chunks
            .values()
            .flatten()
            .filter(|tile| tile.color == legend::COVERAGE_PROVIDER_COLOR)
            .collect();
        assert_eq!(outlines.len(), 4);
        assert!(outlines
            .iter()
            .all(|tile| tile.footprint.center().distance(buildings[2].1) < 25.0));
        assert_eq!(color_at(3), None);
    }

    #[test]
    fn p2_ui_wire_tints_procedural_specials_beyond_their_footprint() {
        use prototypes::{BuildingGen, GoodsCompanyID};
        use scenarios::{builders::grid_city, ScenarioCtx};
        use simulation::SimulationOptions;

        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let grid = grid_city(&mut test, vec2(700.0, 700.0), 4, 4, 160.0);
        let mut map = test.sim.write::<Map>();
        let sites = [
            ("p2-overlay-procedural", vec2(400.0, 450.0)),
            ("cereal-farm", vec2(550.0, 450.0)),
        ];
        for (prototype, center) in sites {
            map.build_special_building(
                &OBB::new(center, Vec2::X, 100.0, 100.0),
                BuildingKind::GoodsCompany(GoodsCompanyID::new(prototype)),
                BuildingGen::CenteredDoor {
                    vertical_factor: 1.0,
                },
                None,
                Some(grid.roads[0]),
            )
            .unwrap();
        }
        drop(map);

        let mut tiles = TileIndex::default();
        collect_tiles(&test.sim, &test.sim.map(), OverlayMode::Power, &mut tiles);
        for (prototype, center) in sites {
            let tile = tiles
                .chunks
                .values()
                .flatten()
                .find(|tile| tile.footprint.center() == center)
                .unwrap();
            assert!(tile.footprint.bbox().w() > 100.0, "{prototype}");
        }
    }

    #[test]
    fn coverage_overlay_version_tracks_per_kind_snapshots() {
        use prototypes::{GameTime, Tick};
        use scenarios::ScenarioCtx;
        use simulation::map_dynamic::coverage_update;
        use simulation::SimulationOptions;

        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let school = OverlayMode::Coverage(ServiceKind::School);
        let health = OverlayMode::Coverage(ServiceKind::Health);

        for tick in [300, 2099, 2100, 2101] {
            test.sim.write::<GameTime>().tick = Tick(tick);
            coverage_update(&mut test.sim);
            let map = test.sim.map();
            let expected = u64::from(tick >= 2100);
            assert_eq!(school.data_version(&test.sim, &map, tick), expected);
            assert_eq!(health.data_version(&test.sim, &map, tick), 1);
        }
    }

    #[test]
    #[ignore = "run in release to measure overlay CPU cost"]
    fn p2e_fix_overlay_10k() {
        use crate::gui::Tool;
        use crate::rendering::{immediate::OrderKind, OrbitCamera};
        use engine::Tesselator;
        use geom::{vec3, Camera, Radians};
        use prototypes::{GameTime, GoodsCompanyID};
        use scenarios::{builders::grid_city, ScenarioCtx};
        use simulation::SimulationOptions;
        use std::time::Instant;

        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 10,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let road = grid_city(&mut test, vec2(4600.0, 4600.0), 2, 2, 160.0).roads[0];
        let bakery = GoodsCompanyID::new("bakery");
        let mut map = test.sim.write::<Map>();
        for x in 0..100 {
            for y in 0..100 {
                let center = vec2(200.0 + 40.0 * x as f32, 240.0 + 40.0 * y as f32);
                map.build_special_building(
                    &OBB::new(center, Vec2::X, 20.0, 20.0),
                    BuildingKind::GoodsCompany(bakery),
                    bakery.prototype().bgen,
                    None,
                    Some(road),
                )
                .unwrap();
            }
        }
        assert_eq!(
            map.buildings()
                .values()
                .filter(|b| b.kind == BuildingKind::GoodsCompany(bakery))
                .count(),
            10_000
        );
        drop(map);

        let mut orbit = OrbitCamera::load((1600, 900));
        orbit.camera = Camera::new(vec3(3000.0, 2000.0, 0.0), 1600.0, 900.0);
        orbit.camera.yaw = Radians(0.0);
        orbit.camera.pitch = Radians(0.28);
        orbit.camera.dist = 1500.0;
        orbit.camera.update();
        let mut ui = UiWorld::default();
        ui.insert(orbit);
        ui.insert(OverlayMode::Power);
        ui.insert(ImmediateDraw::default());
        ui.insert(Tool::Hand);
        draw(&test.sim, &ui);

        for moving in [false, true] {
            let mut samples = Vec::new();
            let (mut vertices, mut indices) = (Vec::new(), Vec::new());
            let (mut orders, mut quads) = (0, 0);
            for frame in 0..250 {
                test.sim.write::<GameTime>().tick.0 += 1;
                if moving {
                    let camera = &mut ui.camera_mut().camera;
                    camera.pos.y = 2000.0 + (frame % 20) as f32;
                    camera.update();
                }
                vertices.clear();
                indices.clear();

                let at = Instant::now();
                draw(&test.sim, &ui);
                let mut tess = Tesselator::new(&mut vertices, &mut indices, None, 1.0);
                ui.camera().cull_tess(&mut tess);
                let immediate = ui.read::<ImmediateDraw>();
                orders = immediate.persistent_orders.len();
                for order in &immediate.persistent_orders {
                    let OrderKind::OBB { obb, z } = order.kind else {
                        unreachable!()
                    };
                    let [width, height] = obb.axis();
                    tess.set_color(order.color);
                    tess.draw_rect_cos_sin(
                        obb.center().z(z),
                        width.mag(),
                        height.mag(),
                        width.normalize(),
                    );
                }
                samples.push(at.elapsed().as_secs_f64() * 1000.0);
                quads = vertices.len() / 4;
            }
            samples.sort_unstable_by(f64::total_cmp);
            println!("moving={moving}, visible={orders}, tessellated={quads}, median={:.3} ms, p95={:.3} ms",
                samples[samples.len() / 2], samples[samples.len() * 95 / 100]);
        }
    }

    #[test]
    fn p2e_legend_maps_binary_and_clamped_overlay_values() {
        assert_eq!(OverlayMode::Off.scale(), None);
        let power = OverlayMode::Power.scale().unwrap();
        assert_eq!(power.labels, ["Blackout / disconnected", "Powered"]);
        assert_ne!(power.color(0.0), power.color(1.0));

        let school = OverlayMode::Coverage(ServiceKind::School).scale().unwrap();
        assert_eq!(school.labels, ["0%", "100%"]);
        assert_eq!(school.color(-1.0), school.color(0.0));
        assert_eq!(school.color(150.0), school.color(100.0));
        assert_ne!(school.color(50.0), school.color(100.0));

        let pollution = OverlayMode::Pollution.scale().unwrap();
        assert_eq!(pollution.labels, ["0", "100+"]);
        assert_ne!(pollution.color(0.0), school.color(0.0));
    }
}

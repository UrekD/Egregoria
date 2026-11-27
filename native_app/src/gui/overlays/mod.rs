mod ground;
pub(crate) mod land_tiles;
mod legend;
pub(crate) use ground::prepare_ground;
pub(crate) use legend::UTILITY_EXPLANATION;
mod traffic;
mod transit;
pub(crate) mod view;
mod water_bodies;

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};

use common::{ChunkID_256, FastMap};
use geom::{InfiniteFrustrum, Intersect3, LinearColor, Radians, Shape, AABB, AABB3, OBB};
use goryak::{combo_box, tooltip};
use prototypes::{try_prototype, GoodsCompanyID, Power, ServiceID, ServiceKind, TICKS_PER_HOUR};
use serde::{Deserialize, Deserializer, Serialize};
use simulation::economy::balance::DEMAND_INTERVAL_TICKS;
use simulation::map::{Building, BuildingID, BuildingKind, Map, MapSubscriber, UpdateType};
use simulation::map_dynamic::{
    active_business, city_field_tick, effective_load, service_reach_sources, service_reach_version,
    BuildingInfos, CoverageGrid, ElectricityFlow, SiteUtilityReader, Transit, UtilityFlow,
    UtilityNetworkReadout, UtilityNetworkState,
};
use simulation::souls::residence::{self, Residences};
use simulation::Simulation;

use crate::gui::{InspectedBuilding, PlacementPreview, Tool};
use crate::rendering::immediate::ImmediateDraw;

use crate::uiworld::UiWorld;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum OverlayMode {
    #[default]
    Off,
    Power,
    Water,
    Sewage,
    WaterBodies,
    Coverage(#[serde(deserialize_with = "deserialize_coverage")] ServiceKind),
    Pollution,
    LandValue,
    Happiness,
    Transit,
    Traffic,
}

pub(crate) fn effective_overlay(
    manual: OverlayMode,
    tool: Tool,
    preview: &PlacementPreview,
) -> OverlayMode {
    if tool == Tool::SpecialBuilding {
        if let Some(mode) = preview.company.and_then(placement_overlay) {
            return mode;
        }
    }
    manual
}

fn placement_overlay(company: GoodsCompanyID) -> Option<OverlayMode> {
    if let Some(service) = try_prototype(ServiceID::from(company)) {
        return Some(match service.service_kind {
            ServiceKind::WaterPump => OverlayMode::Water,
            ServiceKind::SewageOutlet => OverlayMode::Sewage,
            kind => OverlayMode::Coverage(kind),
        });
    }
    let prototype = try_prototype(company)?;
    if prototype
        .power_production
        .is_some_and(|power| power > Power::ZERO)
    {
        Some(OverlayMode::Power)
    } else if prototype.pollution.is_some_and(|pollution| pollution > 0.0) {
        Some(OverlayMode::Pollution)
    } else {
        None
    }
}

pub(crate) fn current_overlay(manual: OverlayMode, uiworld: &UiWorld) -> OverlayMode {
    let tool = *uiworld.read::<Tool>();
    let preview = (tool == Tool::SpecialBuilding).then(|| uiworld.read::<PlacementPreview>());
    effective_overlay(
        manual,
        tool,
        preview.as_deref().unwrap_or(&PlacementPreview::default()),
    )
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
    profiled_power_sites: Vec<BuildingID>,
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
    ground::reset_world_cache();
    water_bodies::reset();
}

#[cfg(test)]
pub(crate) fn world_cache_is_clear() -> bool {
    CACHE.with_borrow(|cache| cache.changes.is_none() && cache.tiles.chunks.is_empty())
        && view::world_cache_is_clear()
        && ground::world_cache_is_clear()
        && water_bodies::world_cache_is_clear()
}

const FOOTPRINT_PADDING: f32 = 6.0;

impl OverlayMode {
    fn data_version(self, sim: &Simulation, map: &Map) -> u64 {
        match self {
            Self::Off | Self::Traffic | Self::WaterBodies => 0,
            Self::Power => power_version(map, &sim.read::<ElectricityFlow>()),
            Self::Transit => transit::invalidation_key(&sim.read::<Transit>()),
            Self::Coverage(kind) => service_reach_version(sim, kind),
            Self::Water | Self::Sewage => {
                let mut hash = DefaultHasher::new();
                (city_field_tick(sim).saturating_sub(1) / DEMAND_INTERVAL_TICKS).hash(&mut hash);
                map.environment().water_revision().hash(&mut hash);
                sim.read::<simulation::world_command::CommandOutcomes>()
                    .sequence
                    .hash(&mut hash);
                // Staffing can change between utility samples.
                (sim.get_tick() / (prototypes::TICKS_PER_REALTIME_SECOND / 2)).hash(&mut hash);
                sim.read::<UtilityFlow>().water_pending(map).hash(&mut hash);
                hash.finish()
            }
            Self::Pollution | Self::LandValue => environment_version(sim),
            Self::Happiness => city_field_tick(sim).saturating_sub(3) / TICKS_PER_HOUR,
        }
    }
}

impl OverlayMode {
    fn is_utility(self) -> bool {
        matches!(self, Self::Power | Self::Water | Self::Sewage)
    }

    fn utility_state(self, readout: &UtilityNetworkReadout) -> UtilityNetworkState {
        match self {
            Self::Power => readout.electricity,
            Self::Water => readout.water,
            Self::Sewage => readout.sewage,
            _ => unreachable!(),
        }
    }
}

fn environment_version(sim: &Simulation) -> u64 {
    city_field_tick(sim).saturating_sub(2) / DEMAND_INTERVAL_TICKS
}

fn power_version(map: &Map, power: &ElectricityFlow) -> u64 {
    let mut hash = DefaultHasher::new();
    UtilityFlow::topology_revision(map).hash(&mut hash);
    for network in map.electricity.networks() {
        let totals = power.network_stats(network.id);
        network.id.hash(&mut hash);
        totals.blackout.hash(&mut hash);
        totals.produced_power.cmp(&Power::ZERO).hash(&mut hash);
        (totals.consumed_power == Power::ZERO).hash(&mut hash);
    }
    hash.finish()
}

fn power_marker_version(sim: &Simulation, map: &Map, profiled_sites: &[BuildingID]) -> u64 {
    let network_version = OverlayMode::Power.data_version(sim, map);
    if profiled_sites.is_empty() {
        return network_version;
    }

    let mut hash = DefaultHasher::new();
    network_version.hash(&mut hash);
    let infos = sim.read::<BuildingInfos>();
    let homes = sim.read::<Residences>();
    for &id in profiled_sites {
        let building = map.get(id).expect("profiled power site");
        if profiled_consumer(sim, building, &infos, &homes, OverlayMode::Power) {
            id.hash(&mut hash);
        }
    }
    hash.finish()
}

pub(crate) fn draw(sim: &Simulation, uiworld: &UiWorld) {
    let mode = current_overlay(*uiworld.read::<OverlayMode>(), uiworld);
    let mut draw = uiworld.write::<ImmediateDraw>();

    CACHE.with_borrow_mut(|cache| {
        if mode == OverlayMode::WaterBodies {
            if cache.mode != mode {
                draw.clear_persistent();
                cache.tiles.clear();
                cache.mode = mode;
            }
            water_bodies::draw(&uiworld.camera().camera, &mut draw);
            return;
        }

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

        if mode == OverlayMode::Power && (map_changed || cache.mode != mode) {
            cache.profiled_power_sites.clear();
            cache.profiled_power_sites.extend(profiled_sites(&map));
        }
        let tick = sim.get_tick();
        let data_version = if mode == OverlayMode::Power {
            power_marker_version(sim, &map, &cache.profiled_power_sites)
        } else {
            mode.data_version(sim, &map)
        };
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
                    transit::draw(
                        &map,
                        &sim.read::<Transit>(),
                        &sim.read::<simulation::map_dynamic::transit::RailTransit>(),
                        &mut draw,
                    );
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

        if let OverlayMode::Coverage(kind) = mode {
            draw_selected_service_ring(uiworld, &map, kind, &mut draw);
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
    crate::gui::traffic_repair::draw_selected(sim, uiworld, &mut draw);
}

fn draw_selected_service_ring(
    uiworld: &UiWorld,
    map: &Map,
    kind: ServiceKind,
    draw: &mut ImmediateDraw,
) {
    if *uiworld.read::<Tool>() == Tool::SpecialBuilding {
        return;
    }
    let Some(building) = uiworld
        .read::<InspectedBuilding>()
        .e
        .and_then(|id| map.get(id))
    else {
        return;
    };
    let Some(service) = building
        .kind
        .as_goods_company()
        .and_then(|id| try_prototype(ServiceID::from(id)))
        .filter(|service| service.service_kind == kind && service.radius_m > 0.0)
    else {
        return;
    };

    const SEGMENTS: usize = 256;
    let center = building.obb.center();
    let points = (0..SEGMENTS)
        .map(|i| {
            let angle = Radians(i as f32 * std::f32::consts::TAU / SEGMENTS as f32);
            let pos = center + angle.vec2() * service.radius_m;
            pos.z(map.environment().height(pos).unwrap_or_default().max(0.0) + 0.4)
        })
        .collect::<Vec<_>>();
    let [r, g, b, a] = legend::MAX_REACH_COLOR;
    draw.polyline(points, 5.0, true)
        .color(LinearColor::new(r, g, b, a));
}

fn collect_tiles(sim: &Simulation, map: &Map, mode: OverlayMode, tiles: &mut TileIndex) {
    profiling::scope!("overlays::collect_tiles");
    tiles.clear();
    let Some(scale) = mode.scale() else {
        return;
    };

    if let OverlayMode::Coverage(kind) = mode {
        let coverage = sim.read::<CoverageGrid>();
        for source in service_reach_sources(sim, kind) {
            let building = &map.buildings()[source.building];
            let color = if source.effective_capacity > 0.0 {
                legend::COVERAGE_PROVIDER_COLOR
            } else {
                legend::INACTIVE_PROVIDER_COLOR
            };
            tiles.push_outline(
                building.obb.expand(2.0 * FOOTPRINT_PADDING),
                building.height + 0.35,
                color,
            );
        }
        for building in map
            .buildings()
            .values()
            .filter(|b| is_coverage_receiver(b.kind))
        {
            tiles.push(
                building.obb.expand(FOOTPRINT_PADDING),
                building.height + 0.35,
                scale.color(coverage.coverage(building.id, kind)),
            );
        }
    }

    if mode.is_utility() {
        collect_utility_markers(sim, map, mode, tiles);
    }
}

fn profiled_sites(map: &Map) -> impl Iterator<Item = BuildingID> + '_ {
    map.buildings()
        .values()
        .filter(|building| building.development().is_some())
        .map(|building| building.id)
}

fn profiled_consumer(
    sim: &Simulation,
    building: &Building,
    infos: &BuildingInfos,
    residences: &Residences,
    mode: OverlayMode,
) -> bool {
    let load = effective_load(
        building,
        residences.by_building.get(&building.id),
        active_business(building, infos, sim.world()),
    );
    match mode {
        OverlayMode::Power => {
            load.residential_power > Power::ZERO || load.business_power > Power::ZERO
        }
        OverlayMode::Water => load.water_demand() > 0.0,
        OverlayMode::Sewage => load.sewage_demand() > 0.0,
        _ => unreachable!(),
    }
}

fn collect_utility_markers(sim: &Simulation, map: &Map, mode: OverlayMode, tiles: &mut TileIndex) {
    let mut utilities = SiteUtilityReader::for_display(sim);
    let mut states = FastMap::default();
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    for building in map.buildings().values() {
        let company = building.kind.as_goods_company().and_then(try_prototype);
        let service = company.and_then(|proto| try_prototype(ServiceID::from(proto.id)));
        let provider = match mode {
            OverlayMode::Power => company
                .and_then(|proto| proto.power_production)
                .is_some_and(|supply| supply > Power::ZERO),
            OverlayMode::Water => service.is_some_and(|p| p.service_kind == ServiceKind::WaterPump),
            OverlayMode::Sewage => {
                service.is_some_and(|p| p.service_kind == ServiceKind::SewageOutlet)
            }
            _ => unreachable!(),
        };
        let consumer = if building.development().is_some() {
            profiled_consumer(sim, building, &infos, &residences, mode)
        } else if mode == OverlayMode::Power {
            matches!(
                building.kind,
                BuildingKind::House | BuildingKind::GoodsCompany(_)
            )
        } else {
            match building.kind {
                BuildingKind::House => residence::uses_water(sim, building.id),
                _ => company
                    .and_then(|p| p.water_consumption)
                    .is_some_and(|rate| rate > 0.0),
            }
        };
        if !provider && !consumer {
            continue;
        }
        let network = map
            .electricity
            .net_id(building.id)
            .filter(|_| !building.is_orphaned());
        let state = *states
            .entry(network)
            .or_insert_with(|| mode.utility_state(&utilities.network_readout(network)));
        let color = legend::utility_color(state);
        let footprint = marker_footprint(building);
        tiles.push(footprint, building.height + 0.35, color);
        if provider {
            tiles.push_outline(
                footprint.expand(FOOTPRINT_PADDING),
                building.height + 0.35,
                legend::COVERAGE_PROVIDER_COLOR,
            );
        }
    }
}

fn marker_footprint(building: &Building) -> OBB {
    match building.kind {
        BuildingKind::GoodsCompany(company)
            if company.is_growable()
                || try_prototype(company)
                    .is_none_or(|proto| proto.asset.is_mesh() || proto.zone.is_some()) =>
        {
            building.obb.expand(FOOTPRINT_PADDING)
        }
        _ => building.obb,
    }
}

pub(crate) fn is_coverage_receiver(kind: BuildingKind) -> bool {
    matches!(kind, BuildingKind::House)
        || kind.as_goods_company().is_some_and(|id| id.is_growable())
}

pub(crate) fn selector(uiworld: &UiWorld, show_unfinished_ui: bool) {
    use OverlayMode::*;

    const MODES: [(OverlayMode, &str); 16] = [
        (Off, "Overlay off"),
        (Power, "Power"),
        (Water, "Water"),
        (Sewage, "Sewage"),
        (WaterBodies, "Water Bodies"),
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
    let mut manual = uiworld.write::<OverlayMode>();
    let mut selected = modes
        .iter()
        .position(|(option, _)| *option == *manual)
        .unwrap_or(0);
    if tooltip(labels[selected], || {
        combo_box(&mut selected, &labels[..count], 170.0)
    }) {
        *manual = modes[selected].0;
    }
    let mode = current_overlay(*manual, uiworld);
    let empty = ground::is_empty(mode);
    match mode {
        WaterBodies => water_bodies::legend(),
        Transit => {
            let empty = uiworld.read::<ImmediateDraw>().persistent_orders.is_empty();
            transit::legend(empty);
        }
        Traffic if !show_traffic => {}
        _ => {
            let label = modes
                .iter()
                .find_map(|&(option, label)| (option == mode).then_some(label))
                .unwrap_or(labels[selected]);
            legend::render(mode, label, empty, uiworld);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec2, Vec2};

    #[test]
    fn water_w4_overlay_toggles_keep_the_original_subscription() {
        use geom::{Camera, Vec3};

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        reset_world_cache();
        let sim = Simulation::new_authored_flat(1, 0).unwrap();
        let mut ui = UiWorld::with_defaults();
        ui.insert(crate::rendering::OrbitCamera::from_camera(Camera::new(
            Vec3::ZERO,
            800.0,
            600.0,
        )));
        *ui.write::<OverlayMode>() = OverlayMode::Power;
        draw(&sim, &ui);
        let mut original = CACHE.with_borrow(|cache| cache.changes.clone().unwrap());
        for _ in 0..3 {
            *ui.write::<OverlayMode>() = OverlayMode::WaterBodies;
            draw(&sim, &ui);
            sim.map().subscribers.dispatch_clear();
            *ui.write::<OverlayMode>() = OverlayMode::Power;
            draw(&sim, &ui);
            assert!(
                !original.take_cleared(),
                "the original subscriber was never drained"
            );
        }
        water_bodies::prepare(&sim.map(), 30);
        assert!(!water_bodies::world_cache_is_clear());
        reset_world_cache();
        assert!(world_cache_is_clear());
    }

    #[test]
    fn u3_placement_overlay_and_manual_restore() {
        use prototypes::GoodsCompanyID;
        use scenarios::ScenarioCtx;
        use simulation::SimulationOptions;

        let _test = ScenarioCtx::new(SimulationOptions::LEGACY);
        let manual = OverlayMode::Transit;
        let mut preview = PlacementPreview::default();
        for (prototype, expected) in [
            ("coal-power-plant", OverlayMode::Power),
            ("flour-factory", OverlayMode::Pollution),
            ("school", OverlayMode::Coverage(ServiceKind::School)),
            ("clinic", OverlayMode::Coverage(ServiceKind::Health)),
            ("police-station", OverlayMode::Coverage(ServiceKind::Police)),
            ("fire-station", OverlayMode::Coverage(ServiceKind::Fire)),
            ("park", OverlayMode::Coverage(ServiceKind::Park)),
            ("water-pump", OverlayMode::Water),
            ("sewage-outlet", OverlayMode::Sewage),
        ] {
            preview.company = Some(GoodsCompanyID::new(prototype));
            assert_eq!(
                effective_overlay(manual, Tool::SpecialBuilding, &preview),
                expected,
                "{prototype}"
            );
            assert_eq!(effective_overlay(manual, Tool::Hand, &preview), manual);
        }
        assert_eq!(
            effective_overlay(manual, Tool::SpecialBuilding, &PlacementPreview::default()),
            manual
        );
    }

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
            .filter(|tile| tile.color == legend::INACTIVE_PROVIDER_COLOR)
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
            ("vegetable-farm", vec2(400.0, 450.0)),
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
    fn zones2_fix_power_markers_follow_staffing_on_a_shared_network() {
        use common::saveload::{Bincode, Encoder};
        use scenarios::ScenarioCtx;
        use simulation::map::{
            Density, GrowableBusinessState, GrowableDevelopment, GrowableSiteState,
            LanePatternBuilder, MapProject, ZoneCell, ZoneKind,
        };
        use simulation::map_dynamic::electricity_flow_system;
        use simulation::souls::goods_company::company_soul;
        use simulation::{HumanID, SimulationOptions, SoulID};

        let _init = ScenarioCtx::new(SimulationOptions::LEGACY);
        let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
        let proto = GoodsCompanyID::new("commercial-l1");
        let (store, house) = {
            let mut map = sim.write::<Map>();
            map.make_connection(
                MapProject::ground(vec2(100.0, 100.0).z(0.0)),
                MapProject::ground(vec2(400.0, 100.0).z(0.0)),
                None,
                &LanePatternBuilder::default().build(),
            )
            .unwrap();
            let lot = map.lots().keys().next().unwrap();
            let store = map
                .place_growable(lot, BuildingKind::GoodsCompany(proto))
                .unwrap();
            // The production placement API deliberately does not commission profiled sites yet.
            let building = map.get(store).unwrap();
            let record = Bincode::encode(building).unwrap();
            let profile = GrowableDevelopment {
                zone: ZoneCell {
                    kind: ZoneKind::Commercial,
                    density: Density::Low,
                },
                level: 1,
                floors: 1,
                roof_z: building.height + 4.0,
                approved_value_band: 0,
                home: None,
                planned_jobs: 12,
                site: GrowableSiteState::Active,
                business: Some(GrowableBusinessState::AwaitingSpawn),
                frozen_shell: None,
            };
            let mut encoded = Bincode::encode(&*map).unwrap();
            let start = encoded
                .windows(record.len())
                .position(|bytes| bytes == record.as_slice())
                .unwrap();
            let option = start + record.len() - 1;
            assert_eq!(encoded[option], 0);
            encoded.splice(option..=option, Bincode::encode(&Some(profile)).unwrap());
            *map = Bincode::decode(&encoded).unwrap();
            let lot = map.lots().keys().next().unwrap();
            (store, map.build_house(lot).unwrap())
        };
        sim.write::<BuildingInfos>().insert(store);
        let Some(SoulID::GoodsCompany(company)) = company_soul(&mut sim, store, proto) else {
            panic!("expected the profiled store to open");
        };
        let net = sim.map().electricity.net_id(store).unwrap();
        assert_eq!(sim.map().electricity.net_id(house), Some(net));

        let mode = OverlayMode::Power;
        let profiled_sites = profiled_sites(&sim.map()).collect::<Vec<_>>();
        assert_eq!(profiled_sites, [store]);
        let sample = |sim: &mut Simulation| {
            let (world, resources) = sim.unrestricted_world_res().unwrap();
            electricity_flow_system(world, resources);
            let map = sim.map();
            let stats = sim.read::<ElectricityFlow>().network_stats(net);
            let mut tiles = TileIndex::default();
            collect_tiles(sim, &map, mode, &mut tiles);
            let center = map.get(store).unwrap().obb.center();
            let marked = tiles
                .chunks
                .values()
                .flatten()
                .any(|tile| tile.footprint.center() == center);
            (
                power_marker_version(sim, &map, &profiled_sites),
                marked,
                stats,
            )
        };
        let (unfilled, marked, before) = sample(&mut sim);
        assert!(!marked);
        sim.world_mut_unchecked().companies[company]
            .workers
            .0
            .push(HumanID::default());
        let (filled, marked, after) = sample(&mut sim);
        assert!(marked);
        assert!(before.consumed_power > Power::ZERO && after.consumed_power > Power::ZERO);
        assert_eq!(
            (before.blackout, before.produced_power),
            (after.blackout, after.produced_power)
        );
        assert_ne!(unfilled, filled);
        sim.world_mut_unchecked().companies[company]
            .workers
            .0
            .clear();
        let (vacated, marked, _) = sample(&mut sim);
        assert!(!marked);
        assert_eq!(vacated, unfilled);
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

        let versions = |sim: &Simulation| {
            let map = sim.map();
            (
                school.data_version(sim, &map),
                health.data_version(sim, &map),
            )
        };
        for (tick, school_updated, health_updated) in [
            (100, false, true),
            (699, false, false),
            (700, true, false),
            (701, false, false),
        ] {
            let (school_before, health_before) = versions(&test.sim);
            test.sim.write::<GameTime>().tick = Tick(tick);
            coverage_update(&mut test.sim);
            let (school_after, health_after) = versions(&test.sim);
            assert_eq!(school_before != school_after, school_updated);
            assert_eq!(health_before != health_after, health_updated);
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

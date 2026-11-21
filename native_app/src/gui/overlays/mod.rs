mod legend;

use std::cell::RefCell;

use geom::{vec2, InfiniteFrustrum, Intersect3, Shape, Vec2, AABB3, OBB};
use goryak::combo_box;
use prototypes::{ServiceKind, TICKS_PER_HOUR};
use serde::{Deserialize, Deserializer};
use simulation::map::{Building, BuildingID, BuildingKind, Map, MapSubscriber, UpdateType};
use simulation::map_dynamic::{CoverageGrid, ElectricityFlow, EnvGrid, UtilityFlow};
use simulation::souls::happiness::Happiness;
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
    tiles: Vec<Tile>,
}

struct Tile {
    footprint: OBB,
    bounds: AABB3,
    color: [f32; 4],
}

thread_local! {
    static CACHE: RefCell<OverlayCache> = RefCell::new(OverlayCache::default());
}

impl Tile {
    fn new(map: &Map, footprint: OBB, value: f32, scale: legend::Scale) -> Option<Self> {
        let z = map.environment.height(footprint.center())? + 0.2;
        let bounds = footprint.bbox();
        Some(Self {
            footprint,
            bounds: AABB3::new(bounds.ll.z(z), bounds.ur.z(z)),
            color: scale.color(value),
        })
    }
}

impl OverlayMode {
    fn data_version(self, tick: u64) -> u64 {
        // Power is refreshed each tick. Coverage refreshes on a staggered 600-tick
        // schedule with offsets on 300-tick boundaries.
        match self {
            Self::Off => 0,
            Self::Power => tick,
            Self::Coverage(_) => tick / 300,
            Self::Water | Self::Sewage | Self::Pollution | Self::LandValue => tick / 3000,
            Self::Happiness => tick / TICKS_PER_HOUR,
        }
    }
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
        let map_changed = changes.take_updated_chunks().next().is_some() || cleared || new_map;

        let tick = sim.get_tick();
        let data_version = mode.data_version(tick);
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

        let camera = uiworld.camera().camera;
        let projection = *camera.proj_cache.as_ref();
        if rebuild || cache.projection != projection {
            cache.projection = projection;
            let view =
                InfiniteFrustrum::from_reversez_invviewproj(camera.eye(), camera.inv_proj_cache);
            draw.clear_persistent();
            for tile in cache
                .tiles
                .iter()
                .filter(|tile| view.intersects(&tile.bounds))
            {
                draw.obb(tile.footprint, tile.bounds.ll.z)
                    .color(tile.color)
                    .persistent();
            }
        }
    });
}

fn collect_tiles(sim: &Simulation, map: &Map, mode: OverlayMode, tiles: &mut Vec<Tile>) {
    tiles.clear();
    let Some(scale) = mode.scale() else {
        return;
    };
    let buildings = |eligible: fn(BuildingKind) -> bool,
                     value: &dyn Fn(BuildingID) -> f32,
                     tiles: &mut Vec<Tile>| {
        tiles.extend(
            map.buildings()
                .values()
                .filter(|b| eligible(b.kind))
                .filter_map(|b| Tile::new(map, b.obb, value(b.id), scale)),
        );
    };

    match mode {
        OverlayMode::Off => {}
        OverlayMode::Power => {
            let flow = sim.read::<ElectricityFlow>();
            buildings(
                has_utilities,
                &|id| f32::from(power_available(map, &flow, id)),
                tiles,
            );
        }
        OverlayMode::Water | OverlayMode::Sewage => {
            let flow = sim.read::<UtilityFlow>();
            buildings(
                has_utilities,
                &|id| {
                    f32::from(if mode == OverlayMode::Water {
                        flow.has_water(id)
                    } else {
                        flow.has_sewage(id)
                    })
                },
                tiles,
            );
        }
        OverlayMode::Coverage(kind) => {
            let coverage = sim.read::<CoverageGrid>();
            buildings(
                is_coverage_receiver,
                &|id| coverage.coverage(id, kind),
                tiles,
            );
        }
        OverlayMode::LandValue => {
            let env = sim.read::<EnvGrid>();
            buildings(
                is_coverage_receiver,
                &|id| f32::from(env.land_value(id)),
                tiles,
            );
        }
        OverlayMode::Pollution => {
            let env = sim.read::<EnvGrid>();
            let size = EnvGrid::CELL_SIZE;
            tiles.extend(env.cells.keys().filter_map(|&(x, y)| {
                let center = vec2(x as f32 + 0.5, y as f32 + 0.5) * size;
                let footprint = OBB::new(center, Vec2::X, size, size);
                Tile::new(map, footprint, env.pollution_at(center), scale)
            }));
        }
        OverlayMode::Happiness => {
            let happiness = sim.read::<Happiness>();
            tiles.extend(happiness.by_building.iter().filter_map(|(&id, &score)| {
                let building = map.get(id)?;
                Tile::new(map, building.obb, f32::from(score), scale)
            }));
        }
    }
}

fn has_utilities(kind: BuildingKind) -> bool {
    matches!(kind, BuildingKind::House | BuildingKind::GoodsCompany(_))
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

pub(crate) fn uses_water(building: &Building) -> bool {
    match building.kind {
        BuildingKind::House => true,
        BuildingKind::GoodsCompany(id) => {
            id.prototype().water_consumption.unwrap_or_default() > 0.0
        }
        _ => false,
    }
}

pub(crate) fn selector(uiworld: &UiWorld) {
    use OverlayMode::*;

    const MODES: [(OverlayMode, &str); 13] = [
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
    ];

    let mut mode = uiworld.write::<OverlayMode>();
    let mut selected = MODES
        .iter()
        .position(|(option, _)| *option == *mode)
        .unwrap_or(0);
    if combo_box(&mut selected, &MODES.map(|(_, label)| label), 200.0) {
        *mode = MODES[selected].0;
    }
    let empty = CACHE.with_borrow(|cache| cache.mode == *mode && cache.tiles.is_empty());
    legend::render(*mode, MODES[selected].1, empty);
}

#[cfg(test)]
mod tests {
    use super::*;

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

use std::collections::BTreeSet;

use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, Polygon, Vec2, AABB, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID};
use simulation::economy::progression::ProgressionMode;
use simulation::map::{
    AuthoredTilesSpec, BuildingKind, GatewayRole, LandSetupError, LandTileId, LanePatternBuilder,
    LotLayout, Map, MapProject, OwnedTileRect, ProtectedRoot, TileExtent,
};
use simulation::Simulation;

pub(super) fn wet_rectangle() -> Result<Simulation, String> {
    let spec = AuthoredTilesSpec {
        recipe_id: 42,
        source_crop: AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(1024.0)),
        town_origin: Vec2::splat(512.0),
        town_scale: 1.0,
        terrain_extent: TileExtent {
            width: 5,
            height: 4,
        },
        owned_rect: OwnedTileRect {
            origin: LandTileId { x: 1, y: 1 },
            width: 3,
            height: 2,
        },
        seed: 7,
        lot_layout: LotLayout::Uniform,
        starting_money_bucks: 200_000,
        progression: ProgressionMode::FreeBuild,
    };

    let (sim, ()) = Simulation::build_authored_tiled(spec, |sim| {
        let lake = Polygon(vec![
            vec2(1780.0, 710.0),
            vec2(2210.0, 604.0),
            vec2(2412.0, 858.0),
            vec2(2380.0, 1210.0),
            vec2(2080.0, 1460.0),
            vec2(1870.0, 1320.0),
            vec2(1920.0, 1000.0),
        ]);
        sim.fill_lake(&lake, 0, 7.0)
            .map_err(|_| LandSetupError::InvalidExtent(spec.terrain_extent))?;

        let mut map = sim.write::<Map>();
        let z = map.environment().height(Vec2::splat(768.0)).unwrap();
        let mut connect = |y, x1, x2, pattern: LanePatternBuilder| {
            map.make_connection(
                MapProject::ground(vec3(x1, y, z)),
                MapProject::ground(vec3(x2, y, z)),
                None,
                &pattern.build(),
            )
            .map(|(_, road)| road)
        };
        let highway = connect(
            256.0,
            8.0,
            2552.0,
            LanePatternBuilder::new().parking(false).sidewalks(false),
        )
        .ok_or(LandSetupError::InvalidGateway(GatewayRole::Highway))?;
        let rail = connect(1768.0, 8.0, 2552.0, LanePatternBuilder::new().rail(true))
            .ok_or(LandSetupError::InvalidGateway(GatewayRole::Rail))?;
        connect(768.0, 664.0, 1392.0, LanePatternBuilder::default())
            .ok_or(LandSetupError::NoBuildableFrontage(spec.owned_rect.origin))?;

        let mut roots = BTreeSet::new();
        for (role, id) in [(GatewayRole::Highway, highway), (GatewayRole::Rail, rail)] {
            let road = map.get(id).ok_or(LandSetupError::InvalidGateway(role))?;
            roots.extend([
                ProtectedRoot::Road(role, id),
                ProtectedRoot::Junction(role, road.src),
                ProtectedRoot::Junction(role, road.dst),
            ]);
        }

        let station =
            BuildingKind::RailFreightStation(FreightStationPrototypeID::new("freight-station"));
        for (kind, x, width, depth) in [
            (BuildingKind::ExternalTrading, 800.0, 72.0, 34.0),
            (station, 1100.0, 100.0, 38.0),
        ] {
            let id = map
                .build_special_building(
                    &OBB::new(vec2(x, 1768.0), Vec2::X, width, depth),
                    kind,
                    BuildingGen::NoWalkway {
                        door_pos: Vec2::ZERO,
                    },
                    None,
                    None,
                )
                .ok_or(LandSetupError::MissingGateway(GatewayRole::Freight))?;
            roots.insert(ProtectedRoot::Building(GatewayRole::Freight, id));
        }
        Ok(((), roots))
    })
    .map_err(|reason| format!("creating the wet tiled fixture: {reason:?}"))?;

    let snapshot = Bincode::encode(&sim).map_err(|err| err.to_string())?;
    Bincode::decode(&snapshot).map_err(|err| err.to_string())
}

#[cfg(test)]
#[test]
fn land_tiles_wet_fixture_reloads_and_quotes_last_column() {
    use simulation::map::{quote_tile, TileStatus};

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let sim = wet_rectangle().unwrap();
    let map = sim.map();
    assert_eq!(map.environment().size(), (5, 4));
    assert!(map.environment().is_water(vec2(2200.0, 1000.0)));
    drop(map);
    let quote = quote_tile(&sim, LandTileId { x: 4, y: 1 }).unwrap();
    assert_eq!(quote.status, TileStatus::Buyable);
    assert_eq!(quote.price.unwrap().bucks(), 10_000);
}

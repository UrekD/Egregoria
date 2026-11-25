use std::collections::BTreeSet;

use geom::{vec2, Vec2, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID};

use crate::map::{
    BuildingKind, GatewayRole, LandOwnership, LandRecipe, LandSetupError, LanePatternBuilder,
    MapProject, OwnedTileRect, ProtectedRoot, TileExtent, LAND_TILE_SIZE,
};
use crate::map_dynamic::BuildingInfos;
use crate::{Simulation, SimulationOptions};

pub(crate) fn commission_native_land(
    sim: &mut Simulation,
    options: SimulationOptions,
) -> Result<(), LandSetupError> {
    let LandRecipe::NativeFlatV1 { lot_layout } = options.land_recipe else {
        return Err(LandSetupError::InvalidMode(options.land_mode));
    };
    let side = options.terrain_size;
    let extent = TileExtent {
        width: side,
        height: side,
    };
    let rect = OwnedTileRect::centered(extent);
    let origin = vec2(rect.origin.x as f32, rect.origin.y as f32) * LAND_TILE_SIZE;
    let edge = f32::from(side) * LAND_TILE_SIZE;
    let mut reserves = BTreeSet::new();
    let mut stations = Vec::new();
    {
        let mut map = sim.map_mut();
        map.set_lot_layout(lot_layout);
        let height = map.environment().height(origin).unwrap_or(0.0);
        let road_end = |p: Vec2| MapProject::ground(p.z(height));
        // Give the external corridors room for end turns and future mutable approaches.
        for (role, y, pattern) in [
            (
                GatewayRole::Highway,
                origin.y - 384.0,
                LanePatternBuilder::new()
                    .n_lanes(2)
                    .sidewalks(false)
                    .parking(false)
                    .speed_limit(28.0)
                    .build(),
            ),
            (
                GatewayRole::Rail,
                origin.y + 840.0,
                LanePatternBuilder::new().rail(true).build(),
            ),
        ] {
            let (_, id) = map
                .make_connection(
                    road_end(vec2(8.0, y)),
                    road_end(vec2(edge - 8.0, y)),
                    None,
                    &pattern,
                )
                .ok_or(LandSetupError::MissingGateway(role))?;
            let road = map.get(id).unwrap();
            reserves.extend([
                ProtectedRoot::Road(role, id),
                ProtectedRoot::Junction(role, road.src),
                ProtectedRoot::Junction(role, road.dst),
            ]);
        }
        for (role, x, kind) in [
            (
                GatewayRole::Freight,
                origin.x - 256.0,
                BuildingKind::ExternalTrading,
            ),
            (
                GatewayRole::Freight,
                origin.x + 512.0,
                BuildingKind::RailFreightStation(FreightStationPrototypeID::new("freight-station")),
            ),
        ] {
            let site = OBB::new(vec2(x, origin.y + 840.0), Vec2::X, 100.0, 38.0);
            let id = map
                .build_special_building(
                    &site,
                    kind,
                    BuildingGen::NoWalkway {
                        door_pos: Vec2::ZERO,
                    },
                    None,
                    None,
                )
                .ok_or(LandSetupError::MissingGateway(role))?;
            reserves.insert(ProtectedRoot::Building(role, id));
            stations.push(id);
        }
        map.install_land_ownership(LandOwnership::tiled(extent, rect, reserves)?);
        let street = LanePatternBuilder::default().build();
        // Both streets are seeded under the same checks as later player construction.
        for y in [origin.y + 240.0, origin.y + 530.0] {
            map.try_connect(
                road_end(vec2(origin.x + 125.0, y)),
                road_end(vec2(origin.x + 935.0, y)),
                None,
                &street,
            )
            .map_err(|_| LandSetupError::NoBuildableFrontage(rect.origin))?;
        }
        let seam_street = LanePatternBuilder::new()
            .one_way(true)
            .parking(false)
            .build();
        let east = origin.x + 980.0;
        map.try_connect(
            road_end(vec2(east, origin.y + 96.0)),
            road_end(vec2(east, origin.y + 468.0)),
            None,
            &seam_street,
        )
        .map_err(|_| LandSetupError::NoBuildableFrontage(rect.origin))?;
        if map.lots().is_empty() {
            return Err(LandSetupError::NoBuildableFrontage(rect.origin));
        }
    }
    for id in stations {
        sim.write::<BuildingInfos>().insert(id);
    }
    Ok(())
}

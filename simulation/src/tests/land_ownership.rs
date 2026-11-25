use crate::map::{
    Density, LanePatternBuilder, MapProject, TerraformKind, WaterBasin, WaterEdit, ZoneGrid,
    ZoneKind,
};
use crate::world_command::{CommandOutcomes, Rejection};
use crate::{MapPreset, NewGameOptions, Replay, Simulation, WorldCommand};
use common::saveload::Encoder;
use geom::{vec2, vec3, Vec2, OBB};

fn options() -> NewGameOptions {
    super::init_test_simulation();
    NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: 16,
        ..NewGameOptions::default()
    }
}

fn gameplay_hashes(sim: &Simulation) -> std::collections::BTreeMap<String, u64> {
    let mut hashes = sim.hashes();
    hashes.remove("replay");
    hashes
}

fn lake(x: f32, y: f32) -> WaterEdit {
    WaterEdit::FillLake {
        basin: WaterBasin::new(&[
            vec2(x, y),
            vec2(x + 64., y),
            vec2(x + 64., y + 64.),
            vec2(x, y + 64.),
        ])
        .unwrap(),
        level_dm: 0,
        depth_m: 5.,
    }
}

#[test]
fn land_ownership_c1_keeps_paused_batches_and_water_prepass_inputs() {
    let mut sim = Simulation::new_game_with_replay(&options()).unwrap();
    let opening = sim.read::<Replay>().command_count();
    let bad = WorldCommand::EditWater(WaterEdit::RemoveFreshWater {
        body: crate::map::WaterBodyId(1),
    });
    let sea = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    });
    let absent = WorldCommand::EditWater(WaterEdit::SetSeaLevel { level_dm: None });
    let raise = WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: vec2(7072., 3072.),
        radius: 48.,
        amount: 100.,
        level: 0.,
        slope: None,
    };
    sim.apply_commands([&bad]);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0).len(), 1);
    sim.apply_commands([&absent]);
    assert!(sim.read::<CommandOutcomes>().rejections_for(0).is_empty());
    let before = gameplay_hashes(&sim);
    sim.apply_commands([&sea, &raise]);
    assert_eq!(
        sim.read::<CommandOutcomes>().rejections_for(1),
        [("Terraform", Rejection::WaterWorkLimit)]
    );
    assert_eq!(gameplay_hashes(&sim), before);
    let mut control = Simulation::new_game(&options()).unwrap();
    let height = control.map().environment().base_height(vec2(7072., 3072.));
    control.apply_commands([&raise]);
    assert_ne!(
        control.map().environment().base_height(vec2(7072., 3072.)),
        height
    );
    let replay = sim.read::<Replay>().clone();
    assert_eq!(replay.command_count(), opening + 4);
    assert_eq!(
        replay.batch_boundaries(),
        [
            (0, opening + 1, false),
            (0, opening + 2, false),
            (0, opening + 4, false),
        ]
    );
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(sim.hashes(), replayed.hashes());
}

// This fixture exercises both command authority and the public Map/ZoneGrid reducers.
#[test]
fn land_ownership_gates_frontier_and_buy_is_immediately_visible() {
    use crate::economy::{Budget, BudgetCategory, Government};
    use crate::map::{LandMode, LandTileId};
    use prototypes::Money;

    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        land_mode: LandMode::Tiled,
        ..options()
    })
    .unwrap();
    let locked = LandTileId { x: 9, y: 7 };
    let area = OBB::new(vec2(4624., 3800.), Vec2::X, 64., 32.);
    let road = WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(4620., 3700., 0.)),
        to: MapProject::ground(vec3(4800., 3700., 0.)),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    };
    let paint = WorldCommand::ZonePaint {
        area,
        kind: Some(ZoneKind::Residential),
        density: Density::Low,
    };
    let raise = WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: vec2(4610., 3900.),
        radius: 40.,
        amount: 100.,
        level: 0.,
        slope: None,
    };
    let fill = WorldCommand::EditWater(lake(4704., 3968.));
    let stop = WorldCommand::TransitAddStop {
        pos: vec3(4704., 3968., 0.),
        dir: Vec2::X,
    };
    let before = gameplay_hashes(&sim);
    sim.apply_commands([&road, &paint, &raise, &fill, &stop]);
    {
        let outcomes = sim.read::<CommandOutcomes>();
        assert!(
            outcomes.accepted.is_empty(),
            "accepted {:?}, denied {:?}",
            outcomes.accepted,
            outcomes.last
        );
    }
    assert_eq!(gameplay_hashes(&sim), before);
    assert!(ZoneGrid::default()
        .paint_on_map(&sim.map(), &area, Some(ZoneKind::Residential), Density::Low)
        .is_err());
    let mut direct = sim.map().clone();
    let WorldCommand::MapMakeConnection {
        from, to, ref pat, ..
    } = road
    else {
        unreachable!()
    };
    assert!(direct.make_connection(from, to, None, pat).is_none());
    let money = sim.read::<Government>().money;
    let buy = WorldCommand::BuyTile { tile: locked };
    let old_lots: Vec<_> = sim.map().lots().keys().collect();
    sim.apply_commands([&buy, &buy, &paint, &road]);
    let results = sim.read::<CommandOutcomes>();
    assert_eq!(
        results.tile_purchase(0).unwrap().unwrap().paid,
        Money::new_bucks(10_000)
    );
    assert_eq!(
        results.tile_purchase(1),
        Some(Err(Rejection::TileAlreadyOwned))
    );
    assert_eq!(results.accepted, [0, 2, 3]);
    assert!(sim.read::<Government>().money <= money - Money::new_bucks(10_000));
    assert_eq!(
        sim.read::<Budget>().total(BudgetCategory::LandPurchase),
        -i128::from(Money::new_bucks(10_000).inner())
    );
    let map = sim.map();
    assert!(map.land_ownership().owns(locked));
    assert!(old_lots.into_iter().all(|id| map.get(id).is_some()));
    assert!(map
        .lots()
        .values()
        .any(|lot| lot.shape.corners.iter().any(|point| point.x > 4608.0)
            && lot.shape.corners.iter().any(|point| point.x < 4608.0)));
    drop(map);
    drop(results);
    let replay = sim.read::<Replay>().clone();
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(sim.hashes(), replayed.hashes());
}

#[test]
fn land_ownership_geometry_uses_closed_footprints_and_half_open_points() {
    use crate::map::{LandDenial, LandOwnership, LandTileId, OwnedTileRect, TileExtent};
    let land = LandOwnership::tiled(
        TileExtent {
            width: 5,
            height: 4,
        },
        OwnedTileRect {
            origin: LandTileId { x: 1, y: 1 },
            width: 2,
            height: 2,
        },
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        land.tile_at(vec2(1536., 514.)),
        Some(LandTileId { x: 3, y: 1 })
    );
    assert_eq!(land.tile_at(vec2(f32::NAN, 512.)), None);
    assert_eq!(land.tile_at(vec2(-0.01, 512.)), None);
    assert_eq!(land.tile_at(vec2(2560., 512.)), None);
    let on_seam = [
        vec2(1504., 544.),
        vec2(1536., 544.),
        vec2(1536., 624.),
        vec2(1504., 624.),
    ];
    assert!(land.check_polygon(&on_seam, None).is_ok());
    let crossed = on_seam.map(|point| point + Vec2::X);
    assert_eq!(
        land.check_polygon(&crossed, None),
        Err(LandDenial::Unowned(LandTileId { x: 3, y: 1 }))
    );
    assert_eq!(
        land.check_polygon(&crossed.map(|point| point + vec2(1024., 0.)), None),
        Err(LandDenial::OutsideMap)
    );
    let mut encoded = crate::map::encode_land(&land).unwrap();
    assert!(crate::map::decode_land(&encoded).is_ok());
    *encoded.last_mut().unwrap() |= 0x80;
    assert!(crate::map::decode_land(&encoded).is_err());
}

#[test]
fn land_ownership_quotes_recorded_milestones_and_persists_buy() {
    use crate::economy::progression::{ProgressionMode, ProgressionState};
    use crate::map::{quote_tile, LandMode, LandTileId, LotLayout, TileStatus};
    use crate::{SlotId, SnapshotRevision};
    use prototypes::Money;

    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        terrain_chunks: 17,
        land_mode: LandMode::Tiled,
        progression: ProgressionMode::Guided,
        lot_layout: LotLayout::Uniform,
        ..options()
    })
    .unwrap();
    let first = LandTileId { x: 9, y: 7 };
    let second = LandTileId { x: 10, y: 7 };
    let quote = |sim: &Simulation, tile| quote_tile(sim, tile).unwrap();
    let owned: Vec<_> = sim
        .map()
        .land_ownership()
        .tiles()
        .filter(|&tile| sim.map().land_ownership().owns(tile))
        .collect();
    assert_eq!(
        owned,
        [
            LandTileId { x: 7, y: 7 },
            LandTileId { x: 8, y: 7 },
            LandTileId { x: 7, y: 8 },
            LandTileId { x: 8, y: 8 },
        ]
    );
    assert_eq!(
        quote(&sim, first).status,
        TileStatus::Locked {
            milestone: 1,
            agents: 100
        }
    );
    assert_eq!(quote(&sim, second).status, TileStatus::NeedsAdjacent);
    sim.apply_commands([&WorldCommand::BuyTile { tile: first }]);
    assert_eq!(
        sim.read::<CommandOutcomes>().tile_purchase(0),
        Some(Err(Rejection::Locked { milestone: 1 }))
    );

    // The recorded tier continues to authorize expansion after a population decline.
    sim.write::<ProgressionState>().highest_milestone = 1;
    sim.apply_commands([
        &WorldCommand::BuyTile { tile: first },
        &WorldCommand::BuyTile { tile: second },
    ]);
    assert_eq!(
        sim.read::<CommandOutcomes>()
            .tile_purchase(1)
            .unwrap()
            .unwrap()
            .paid,
        Money::new_bucks(10_500)
    );
    assert_eq!(
        quote(&sim, LandTileId { x: 11, y: 7 }).status,
        TileStatus::Locked {
            milestone: 2,
            agents: 250
        }
    );

    let root = std::env::temp_dir().join(format!("egregoria-tile-slot-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let slot = SlotId::new("tiles").unwrap();
    sim.save_to_slot(&root, &slot, SnapshotRevision::new(1))
        .unwrap();
    let loaded = Simulation::load_from_slot(&root, &slot);
    std::fs::remove_dir_all(root).unwrap();
    let mut loaded = loaded.unwrap();
    assert_eq!(sim.hashes(), loaded.hashes());
    assert!(loaded.map().land_ownership().owns(second));
    assert_eq!(loaded.map().lot_layout(), LotLayout::Uniform);
    loaded.apply_commands([&WorldCommand::SetLotLayout(LotLayout::Varied)]);
    assert!(matches!(
        loaded.read::<CommandOutcomes>().rejections_for(0),
        [(_, Rejection::LandDenied(_))]
    ));
}

#[test]
fn land_ownership_protected_roads_buildings_and_transit_are_immutable() {
    use crate::map::{BuildingKind, GatewayRole, LandDenial, LandMode, ProtectedRoot};
    use prototypes::{BuildingGen, FreightStationPrototypeID};

    let mut sim = Simulation::new_game(&NewGameOptions {
        land_mode: LandMode::Tiled,
        ..options()
    })
    .unwrap();
    let (id, src, point, building, before) = {
        let map = sim.map();
        let id = map
            .land_ownership()
            .reserves()
            .find_map(|root| match root {
                ProtectedRoot::Road(GatewayRole::Rail, id) => Some(id),
                _ => None,
            })
            .unwrap();
        let road = map.get(id).unwrap();
        let point = road.points().point_along(4100.0);
        let building = map
            .land_ownership()
            .reserves()
            .find_map(|root| match root {
                ProtectedRoot::Building(_, id) => Some(id),
                _ => None,
            })
            .unwrap();
        (id, road.src, point, building, sim.hashes())
    };
    let place = WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(vec2(4700., 3810.), Vec2::X, 40., 28.),
        kind: BuildingKind::RailFreightStation(FreightStationPrototypeID::new("freight-station")),
        gen: BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        },
        zone: None,
        connected_road: None,
    };
    assert!(matches!(
        place.preview(&sim),
        Err(Rejection::LandDenied(LandDenial::Unowned(_)))
    ));
    let edits = [
        WorldCommand::MapRemoveRoad(id),
        WorldCommand::MapRemoveIntersection(src),
        WorldCommand::MapRemoveBuilding(building),
        WorldCommand::MapMakeConnection {
            from: MapProject {
                pos: point,
                kind: id.into(),
            },
            to: MapProject::ground(vec3(4290., 4300., 0.)),
            inter: None,
            pat: LanePatternBuilder::new().rail(true).build(),
        },
        place,
    ];
    sim.apply_commands(&edits);
    let outcomes = sim.read::<CommandOutcomes>();
    assert!(outcomes.accepted.is_empty(), "{:?}", outcomes.last);
    assert_eq!(
        outcomes.rejections_for(0),
        [(
            "MapRemoveRoad",
            Rejection::LandDenied(LandDenial::ProtectedGateway)
        )]
    );
    assert_eq!(sim.hashes(), before);
    let mut direct = sim.map().clone();
    assert!(direct.remove_road(id).is_none());
    direct.remove_intersection(src);
    assert!(direct.get(src).is_some());
    assert!(direct.remove_building(building).is_none());
    let WorldCommand::MapBuildSpecialBuilding { pos, kind, gen, .. } = edits.last().unwrap() else {
        unreachable!()
    };
    assert!(direct
        .build_special_building(pos, *kind, *gen, None, None)
        .is_none());
}

#[test]
fn land_ownership_x1_owned_wet_stamp_and_locked_shoreline() {
    use crate::map::{LandMode, ZonePaintError};
    let mut sim = Simulation::new_game(&NewGameOptions {
        land_mode: LandMode::Tiled,
        ..options()
    })
    .unwrap();
    sim.edit_water(lake(4528., 3600.)).unwrap();
    let paint = |area, kind| WorldCommand::ZonePaint {
        area,
        kind,
        density: Density::Low,
    };
    let residential = Some(ZoneKind::Residential);
    let coast = OBB::new(vec2(4544., 3616.), Vec2::X, 80., 24.);
    let mixed =
        ZoneGrid::eligible_paint_cells(&sim.map(), &coast, residential, Density::Low).unwrap();
    let full = ZoneGrid::eligible_paint_cells(&sim.map(), &coast, None, Density::Low).unwrap();
    assert!(!mixed.cells().is_empty() && full.cells().len() > mixed.cells().len());
    sim.apply_commands([&paint(coast, residential)]);
    let grid = sim.read::<ZoneGrid>();
    assert_eq!(grid.cells().len(), mixed.cells().len());
    for cell in mixed.cells() {
        assert!(grid.cells().contains_key(cell));
    }
    drop(grid);
    let seam = OBB::new(vec2(4592., 3632.), Vec2::X, 80., 16.);
    let before = sim.hashes();
    assert!(matches!(
        ZoneGrid::eligible_paint_cells(&sim.map(), &seam, residential, Density::Low),
        Err(ZonePaintError::TileLocked(_))
    ));
    sim.apply_commands([&paint(seam, residential)]);
    assert_eq!(
        sim.read::<CommandOutcomes>().rejections_for(0),
        [("ZonePaint", Rejection::TileLocked)]
    );
    assert_eq!(sim.hashes(), before);
    sim.apply_commands([&paint(coast, None)]);
    assert!(sim.read::<ZoneGrid>().cells().is_empty());
}

#[test]
fn land_ownership_authored_rectangle_is_snapshot_only() {
    use crate::economy::progression::ProgressionMode;
    use crate::map::{
        AuthoredTilesSpec, BuildingKind, GatewayRole, LandSetupError, LandTileId, LotLayout,
        OwnedTileRect, ProtectedRoot, TileExtent,
    };
    use geom::AABB;
    use prototypes::BuildingGen;
    use std::collections::BTreeSet;

    super::init_test_simulation();
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
            width: 2,
            height: 2,
        },
        seed: 7,
        lot_layout: LotLayout::Uniform,
        starting_money_bucks: 200_000,
        progression: ProgressionMode::FreeBuild,
    };
    let (mut sim, ()) = Simulation::build_authored_tiled(spec, |sim| {
        let mut map = sim.map_mut();
        let z = map.environment().height(Vec2::splat(768.0)).unwrap();
        let mut connect = |y, x1, x2, pattern: LanePatternBuilder| {
            map.make_connection(
                MapProject::ground(vec3(x1, y, z)),
                MapProject::ground(vec3(x2, y, z)),
                None,
                &pattern.build(),
            )
            .unwrap()
            .1
        };
        let highway = connect(
            256.0,
            8.0,
            2552.0,
            LanePatternBuilder::new().parking(false).sidewalks(false),
        );
        let rail = connect(1768.0, 8.0, 2552.0, LanePatternBuilder::new().rail(true));
        connect(768.0, 664.0, 1392.0, LanePatternBuilder::default());
        let mut roots = BTreeSet::new();
        for (role, id) in [(GatewayRole::Highway, highway), (GatewayRole::Rail, rail)] {
            let road = map.get(id).unwrap();
            roots.extend([
                ProtectedRoot::Road(role, id),
                ProtectedRoot::Junction(role, road.src),
                ProtectedRoot::Junction(role, road.dst),
            ]);
        }
        let market = map
            .build_special_building(
                &OBB::new(vec2(800.0, 1768.0), Vec2::X, 72.0, 34.0),
                BuildingKind::ExternalTrading,
                BuildingGen::NoWalkway {
                    door_pos: Vec2::ZERO,
                },
                None,
                None,
            )
            .ok_or(LandSetupError::MissingGateway(GatewayRole::Freight))?;
        roots.insert(ProtectedRoot::Building(GatewayRole::Freight, market));
        let station = map
            .build_special_building(
                &OBB::new(vec2(1100.0, 1768.0), Vec2::X, 100.0, 38.0),
                BuildingKind::RailFreightStation(prototypes::FreightStationPrototypeID::new(
                    "freight-station",
                )),
                BuildingGen::NoWalkway {
                    door_pos: Vec2::ZERO,
                },
                None,
                None,
            )
            .ok_or(LandSetupError::MissingGateway(GatewayRole::Freight))?;
        roots.insert(ProtectedRoot::Building(GatewayRole::Freight, station));
        Ok(((), roots))
    })
    .unwrap();
    assert_eq!(sim.map().environment().size(), (5, 4));
    assert_eq!(
        sim.map().land_ownership().extent(),
        Some(spec.terrain_extent)
    );
    assert!(!sim.read::<Replay>().enabled);
    let buy = WorldCommand::BuyTile {
        tile: LandTileId { x: 3, y: 1 },
    };
    sim.apply_commands([&buy]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    let bytes = common::saveload::Bincode::encode(&sim).unwrap();
    let loaded: Simulation = common::saveload::Bincode::decode(&bytes).unwrap();
    assert_eq!(sim.hashes(), loaded.hashes());
}

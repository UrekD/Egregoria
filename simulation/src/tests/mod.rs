#![allow(dead_code)]
#![cfg(test)]

use crate::map::{BuildingID, LanePatternBuilder, ProjectFilter};
use crate::map_dynamic::BuildingInfos;
use crate::utils::scheduler::SeqSchedule;
use crate::world_command::WorldCommand;
use crate::{Simulation, SimulationOptions};
use common::logger::MyLog;
use common::saveload::Encoder;
use geom::{Vec2, Vec3};
use std::sync::Once;

mod citymap_bootstrap;
mod lifecycle;
mod p1_fix2;
mod p2_services;
mod p2abc_fix;
mod p3a_transit;
mod p3_spine;
mod p5_bridge;
mod p5_core_loop;
mod p5_placement;
mod p5_spine_followups;
mod test_iso;
mod vehicles;
mod zone2_lot_layout;

#[test]
fn w0_water_height_free_change_records_once_and_retry_is_clean() {
    use crate::map::{WaterDirtyChunks, WaterEdit};
    use crate::{MapPreset, NewGameOptions, Replay};

    init_test_simulation();
    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        ..NewGameOptions::default()
    })
    .unwrap();
    let initial_revision = sim.map().environment().water_revision();
    let initial_commands = sim.read::<Replay>().command_count();
    let edit = WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    };

    let outcome = sim.edit_water(edit.clone()).unwrap();
    assert!(outcome.changed);
    assert!(outcome.dirty.terrain().is_empty());
    assert_eq!(sim.map().environment().sea_level_dm(), Some(-100));
    assert_eq!(
        sim.map().environment().water_revision(),
        initial_revision + 1
    );
    {
        let replay = sim.read::<Replay>();
        assert_eq!(replay.command_count(), initial_commands + 1);
        assert!(matches!(
            replay
                .commands(initial_commands..initial_commands + 1)
                .next(),
            Some(WorldCommand::EditWater(WaterEdit::SetSeaLevel {
                level_dm: Some(-100)
            }))
        ));
    }

    let retry = sim.edit_water(edit).unwrap();
    assert!(!retry.changed);
    assert_eq!(retry.dirty, WaterDirtyChunks::default());
    assert_eq!(
        sim.map().environment().water_revision(),
        initial_revision + 1
    );
    assert_eq!(sim.read::<Replay>().command_count(), initial_commands + 1);

    let replay = sim.read::<Replay>().clone();
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(replayed.map().environment().sea_level_dm(), Some(-100));
    assert_eq!(
        replayed.map().environment().water_revision(),
        initial_revision + 1
    );
}

#[test]
fn w0_water_batch_reserves_one_sea_rebuild_and_preserves_dry_commands() {
    use crate::map::{TerraformKind, WaterBodyId, WaterEdit, WaterEditError};
    use crate::world_command::CommandOutcomes;
    use prototypes::GameTime;

    let mut ctx = TestCtx::new();
    let first = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    });
    let second = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
        level_dm: Some(-101),
    });
    let dry = WorldCommand::SetGameTime(*ctx.g.read::<GameTime>());
    ctx.g.apply_commands([&first, &dry, &second]);
    assert_eq!(ctx.g.map().environment().sea_level_dm(), None);
    let outcomes = ctx.g.read::<CommandOutcomes>();
    assert_eq!(outcomes.accepted, [1]);
    for index in [0, 2] {
        assert_eq!(
            outcomes.water_edit(index),
            Some(&Err(WaterEditError::Limit))
        );
    }
    assert!(outcomes.water_edit(1).is_none());
    drop(outcomes);

    let remove = WorldCommand::EditWater(WaterEdit::RemoveFreshWater {
        body: WaterBodyId(1),
    });
    let terraform = WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: Vec2::splat(160.0),
        radius: 24.0,
        amount: -2.0,
        level: 0.0,
        slope: None,
    };
    ctx.g.apply_commands([&remove, &terraform]);
    let outcomes = ctx.g.read::<CommandOutcomes>();
    assert_eq!(outcomes.accepted, [1]);
    assert_eq!(
        outcomes.water_edit(0),
        Some(&Err(WaterEditError::BodyUnavailable(WaterBodyId(1))))
    );
}

#[test]
fn w0_water_full_dispatch_covers_odd_rectangular_maps() {
    use crate::map::{Environment, Map, SubscriberChunkID, UpdateType};

    let mut map = Map::empty();
    assert!(map.install_environment(Environment::new(5, 3)));
    let mut water = map.subscribe(UpdateType::Water);
    let mut support = map.subscribe(UpdateType::RoadSupport);
    map.dispatch_all();
    let expected: Vec<_> = (0..3)
        .flat_map(|x| (0..2).map(move |y| SubscriberChunkID::new_i16(x, y)))
        .collect();
    assert!(water.take_cleared());
    assert!(support.take_cleared());
    assert_eq!(water.take_updated_chunks().collect::<Vec<_>>(), expected);
    assert_eq!(support.take_updated_chunks().collect::<Vec<_>>(), expected);

    let map = Map::empty();
    let mut water = map.subscribe(UpdateType::Water);
    map.dispatch_all();
    assert!(water.take_cleared());
    assert_eq!(water.take_updated_chunks().count(), 0);
}

#[test]
fn w0_water_zoning_checks_land_before_water_and_votes_from_prepared_cells() {
    use crate::map::{Density, WaterBasin, WaterEdit, ZoneGrid, ZoneKind, ZonePaintError};
    use geom::{vec2, OBB};

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(5, 0).unwrap();
    sim.edit_water(WaterEdit::FillLake {
        basin: WaterBasin::new(&[
            vec2(1.0, 1.0),
            vec2(15.0, 1.0),
            vec2(15.0, 15.0),
            vec2(1.0, 15.0),
        ])
        .unwrap(),
        level_dm: 50,
        depth_m: 1.0,
    })
    .unwrap();
    let mixed = OBB::new(vec2(16.0, 8.0), Vec2::X, 32.0, 16.0);
    let wet = OBB::new(vec2(8.0, 8.0), Vec2::X, 16.0, 16.0);
    let kind = Some(ZoneKind::Residential);
    let map = sim.map();
    let locked = ZoneGrid::eligible_paint_cells_with_land(&map, &mixed, kind, |key| key != (0, 0));
    assert!(matches!(locked, Err(ZonePaintError::TileLocked((0, 0)))));
    assert!(matches!(
        ZoneGrid::eligible_paint_cells(&map, &wet, kind),
        Err(ZonePaintError::WetFootprint)
    ));
    drop(map);
    for kind in [kind, None] {
        for locked in [(0, 0), (2, 0)] {
            let before = sim.read::<ZoneGrid>().generation();
            assert_eq!(
                WorldCommand::apply_zone_paint(&sim, &mixed, kind, Density::Low, |key| key
                    != locked),
                Err(crate::world_command::Rejection::TileLocked),
            );
            let grid = sim.read::<ZoneGrid>();
            assert_eq!(grid.generation(), before);
            assert!(grid.cells().is_empty());
        }
    }
    let map = sim.map();
    let paint = ZoneGrid::eligible_paint_cells(&map, &mixed, kind).unwrap();
    let erase = ZoneGrid::eligible_paint_cells(&map, &mixed, None).unwrap();
    assert_eq!(paint.cells(), &[(2, 0), (2, 1), (3, 0), (3, 1)]);
    assert_eq!(erase.cells().len(), 8);
    drop(map);

    for (prepared, kind) in [(paint, kind), (erase, None)] {
        let vote =
            sim.read::<ZoneGrid>()
                .lot_zone_after_prepared_paint(&mixed, &prepared, Density::Low);
        sim.apply_commands([&WorldCommand::ZonePaint {
            area: mixed,
            kind,
            density: Density::Low,
        }]);
        let grid = sim.read::<ZoneGrid>();
        assert_eq!(grid.lot_zone(&mixed), vote.1);
        assert_eq!(grid.cells().len(), if kind.is_some() { 4 } else { 0 });
    }
}

#[test]
fn w0_water_rejects_protected_footprints_without_side_effects() {
    use crate::map::{
        BuildingKind, ProjectKind, UpdateType, WaterBasin, WaterEdit, WaterEditError, Zone,
    };
    use crate::world_command::CommandOutcomes;
    use geom::{vec2, Polygon, OBB};
    use prototypes::BuildingGen;

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(5, 0).unwrap();
    let far_zone = Zone::new(
        Polygon::centered_rect(vec2(700.0, 700.0), 32.0, 32.0),
        Vec2::X,
    );
    let body = sim
        .map_mut()
        .build_special_building(
            &OBB::new(vec2(200.0, 200.0), Vec2::X, 16.0, 16.0),
            BuildingKind::ExternalTrading,
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            Some(far_zone),
            None,
        )
        .unwrap();
    let mut dirty = sim
        .map()
        .subscribe(UpdateType::Water | UpdateType::Terrain | UpdateType::RoadSupport);
    let baseline = sim.hashes();
    let old_sequence = sim.read::<CommandOutcomes>().batch_seq();

    for center in [vec2(200.0, 200.0), vec2(700.0, 700.0)] {
        let footprint = Polygon::centered_rect(center, 6.0, 6.0);
        let edit = WaterEdit::FillLake {
            basin: WaterBasin::new(&footprint.0).unwrap(),
            level_dm: 50,
            depth_m: 1.0,
        };
        assert!(matches!(
            sim.edit_water(edit),
            Err(WaterEditError::OccupiedFootprint { object: ProjectKind::Building(id), .. }) if id == body
        ));
        assert_eq!(sim.hashes(), baseline);
        assert_eq!(dirty.take_updated_chunks().count(), 0);
    }
    assert_ne!(sim.read::<CommandOutcomes>().batch_seq(), old_sequence);
}

#[test]
fn w0_water_waters_bootstrap_restores_from_seeded_init() {
    use crate::{MapPreset, NewGameOptions, Replay};

    init_test_simulation();
    let options = NewGameOptions {
        terrain_chunks: 16,
        preset: MapPreset::Waters,
        seed: 77,
        ..NewGameOptions::default()
    };
    let sim = Simulation::new_game_with_replay(&options).unwrap();
    assert!(sim.map().environment().strict_water_siting());
    let replay = sim.read::<Replay>().clone();
    let (replayed, _) = Simulation::try_from_replay(replay).unwrap();
    assert_eq!(sim.hashes()["map"], replayed.hashes()["map"]);
}

#[test]
fn w0_water_guarded_terraform_survives_slot_and_replay_boundaries() {
    use crate::map::{MapProject, TerraformKind, WaterEdit};
    use crate::map_dynamic::UtilityFlow;
    use crate::world_command::{CommandOutcomes, Rejection};
    use crate::{MapPreset, NewGameOptions, Replay};
    use common::saveload::{SlotId, SnapshotRevision};
    use geom::vec2;

    struct TestSlot(std::path::PathBuf);
    impl Drop for TestSlot {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let root = TestSlot(std::env::temp_dir().join(format!("egr-water-w0-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    let slot = SlotId::new("water-w0").unwrap();

    init_test_simulation();
    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        ..NewGameOptions::default()
    })
    .unwrap();
    let mut schedule = Simulation::schedule();
    let sea = WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    };
    sim.edit_water(sea.clone()).unwrap();
    let previous_commands = sim.read::<Replay>().command_count();
    let too_many_projects = WorldCommand::MapMakeMultipleConnections(
        vec![MapProject::ground(Vec3::ZERO); 65_537],
        Vec::new(),
    );
    sim.apply_commands([&too_many_projects]);
    assert_eq!(sim.read::<Replay>().command_count(), previous_commands);
    assert_eq!(
        sim.read::<CommandOutcomes>().rejections_for(0),
        [("MapMakeMultipleConnections", Rejection::WaterWorkLimit)]
    );

    let pending = sim.map().pending_construction_cuts.clone();
    assert!(!pending.is_empty());
    sim.map().dispatch_all();
    assert_eq!(sim.map().pending_construction_cuts, pending);
    sim.save_to_slot(&root.0, &slot, SnapshotRevision::new(1))
        .unwrap();
    let mut loaded = Simulation::load_from_slot(&root.0, &slot).unwrap();
    assert_eq!(loaded.hashes(), sim.hashes());
    assert!(loaded.read::<UtilityFlow>().water_pending(&loaded.map()));

    let terraform = WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: vec2(160.0, 160.0),
        radius: 24.0,
        amount: -2.0,
        level: 0.0,
        slope: None,
    };
    let before_tick = loaded.get_tick();
    let before_height = loaded.map().environment().base_height(vec2(160.0, 160.0));
    loaded.tick(&mut schedule, [&WorldCommand::EditWater(sea), &terraform]);
    assert_eq!(loaded.get_tick(), before_tick + 1);
    assert_eq!(
        loaded.read::<Replay>().command_count(),
        previous_commands + 1
    );
    let after_height = loaded.map().environment().base_height(vec2(160.0, 160.0));
    assert!(after_height < before_height);
    loaded
        .save_to_slot(&root.0, &slot, SnapshotRevision::new(2))
        .unwrap();

    let replay = Simulation::load_replay_from_slot(&root.0, &slot).unwrap();
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(loaded.hashes(), replayed.hashes());
}

fn init_test_simulation() {
    // SAFETY: Simulation unit tests run serially because they replace the global registry.
    unsafe { crate::init::load_prototypes() };
    crate::init::register_simulation();
}

pub(crate) struct TestCtx {
    pub g: Simulation,
    sched: SeqSchedule,
}

impl TestCtx {
    pub(crate) fn new() -> Self {
        static INIT_LOGGER: Once = Once::new();
        INIT_LOGGER.call_once(MyLog::init);
        init_test_simulation();

        let g = Simulation::new_with_options(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let sched = Simulation::schedule();

        Self { g, sched }
    }

    pub(crate) fn new_fast() -> Self {
        Self::new()
    }

    pub(crate) fn build_roads(&self, v: &[Vec3]) {
        let mut m = self.g.map_mut();
        for w in v.windows(2) {
            let a = m.project(w[0], 0.0, ProjectFilter::ALL);
            let b = m.project(w[1], 0.0, ProjectFilter::ALL);
            m.make_connection(a, b, None, &LanePatternBuilder::default().build());
        }
    }

    pub(crate) fn build_house_near(&self, p: Vec2) -> BuildingID {
        let lot = self
            .g
            .map()
            .lots()
            .values()
            .min_by_key(|lot| lot.shape.center().distance2(p) as i32)
            .unwrap()
            .id;

        let b = self.g.map_mut().build_house(lot).unwrap();
        self.g.write::<BuildingInfos>().insert(b);
        crate::souls::residence::on_house_built(&self.g, b);
        b
    }

    pub(crate) fn set_residential_demand(&self, demand: f32) {
        self.g
            .write::<crate::economy::ZoneDemand>()
            .values
            .insert(crate::map::ZoneKind::Residential, demand);
    }

    pub(crate) fn apply(&mut self, commands: &[WorldCommand]) {
        for c in commands {
            c.apply(&mut self.g);
        }
    }

    pub(crate) fn tick_fast(&mut self) {
        self.g.tick(&mut self.sched, &[]);
    }

    pub(crate) fn tick(&mut self) {
        self.tick_fast();

        let serialized = common::saveload::Bincode::encode(&self.g).unwrap();
        let deserialized: Simulation = common::saveload::Bincode::decode(&serialized).unwrap();

        let testhashes = self.g.hashes();
        for (key, hash) in deserialized.hashes().iter() {
            assert_eq!(
                testhashes.get(key),
                Some(hash),
                "key: {:?} at tick {}",
                key,
                self.g.get_tick(),
            );
        }
    }
}

#[test]
fn fast_tick_skips_serde() {
    let mut fast = TestCtx::new_fast();
    let fast_tick = fast.g.get_tick();

    // A tick can run without this resource, but serializing the simulation requires it.
    let (_, resources) = fast.g.world_res();
    assert!(resources.remove::<SimulationOptions>().is_some());

    fast.tick_fast();
    fast.tick_fast();

    assert_eq!(fast.g.get_tick(), fast_tick + 2);
}

#[test]
fn w0_water_rejects_excess_lanes_before_topology_and_replay() {
    use crate::map::{LaneKind, LanePattern, MapProject, WaterEdit};
    use crate::world_command::{CommandOutcomes, Rejection};
    use crate::Replay;
    use geom::vec3;

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
    sim.edit_water(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    })
    .unwrap();
    sim.write::<Replay>().enabled = true;
    let from = MapProject::ground(vec3(32.0, 96.0, 10.0));
    let to = MapProject::ground(vec3(160.0, 96.0, 10.0));
    let pat = LanePattern {
        lanes_forward: vec![(LaneKind::Driving, 10.0); 17],
        lanes_backward: vec![(LaneKind::Driving, 10.0); 16],
    };
    let single = WorldCommand::MapMakeConnection {
        from,
        to,
        inter: None,
        pat: pat.clone(),
    };
    let indexed = WorldCommand::MapMakeMultipleConnections(vec![from, to], vec![(0, 1, None, pat)]);
    for command in [single, indexed] {
        sim.apply_commands([&command]);
        assert_eq!(
            sim.read::<CommandOutcomes>().last[0].1,
            Rejection::WaterWorkLimit
        );
        assert_eq!(sim.map().roads().len(), 0);
        assert_eq!(sim.read::<Replay>().command_count(), 0);
    }
}

#[test]
fn w0_water_removing_last_cut_contributor_restores_base() {
    use crate::map::{BuildingKind, Environment, Map, WaterBasin, WaterEdit};
    use geom::{vec2, OBB};
    use prototypes::BuildingGen;

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(5, 0).unwrap();
    let mut terrain = Environment::new(5, 3);
    terrain.terrain_apply(terrain.bounds(), |p| if p.x >= 1056.0 { 0.0 } else { 10.0 });
    let mut map = Map::empty();
    assert!(map.install_environment(terrain));
    *sim.write::<Map>() = map;
    let at = vec2(1064.0, 1048.0);
    let building = sim
        .map_mut()
        .build_special_building(
            &OBB::new(at, Vec2::X, 16.0, 16.0),
            BuildingKind::ExternalTrading,
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            None,
            None,
        )
        .unwrap();
    let node = vec2(1040.0, 1040.0);
    let base = sim.map().environment().base_height(node);
    sim.map_mut().update();
    assert_ne!(sim.map().environment().true_height(node), base);
    sim.apply_commands([&WorldCommand::MapRemoveBuilding(building)]);
    sim.edit_water(WaterEdit::FillLake {
        basin: WaterBasin::new(&[
            node + vec2(1.0, 1.0),
            node + vec2(15.0, 1.0),
            node + vec2(15.0, 15.0),
            node + vec2(1.0, 15.0),
        ])
        .unwrap(),
        level_dm: 50,
        depth_m: 1.0,
    })
    .unwrap();
    {
        let map = sim.map();
        assert_eq!(
            map.environment().true_height(node),
            map.environment().base_height(node)
        );
    }
    sim.map_mut().update();
    let dry = vec2(1040.0, 1056.0 + 16.0);
    let map = sim.map();
    assert_eq!(
        map.environment().true_height(dry),
        map.environment().base_height(dry)
    );
}

#[test]
fn w0_water_detached_zone_has_two_spatial_entries() {
    use crate::map::{BuildingKind, Zone};
    use geom::{vec2, Polygon, OBB};
    use prototypes::BuildingGen;

    init_test_simulation();
    let sim = Simulation::new_authored_flat(5, 0).unwrap();
    let mut map = sim.map_mut();
    let obb = OBB::new(vec2(200.0, 200.0), Vec2::X, 16.0, 16.0);
    let polygon = Polygon::centered_rect(vec2(700.0, 700.0), 32.0, 32.0);
    let building = map
        .build_special_building(
            &obb,
            BuildingKind::ExternalTrading,
            BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            Some(Zone::new(polygon, Vec2::X)),
            None,
        )
        .unwrap();
    assert_eq!(map.spatial_map().debug_grid().count(), 2);
    use geom::Shape;
    assert_eq!(
        map.spatial_map
            .construction_candidates(map.environment().bounds(), ProjectFilter::BUILDING,)
            .collect::<Vec<_>>(),
        [building.into()]
    );
    assert_eq!(
        map.spatial_map
            .construction_candidates(
                OBB::new(vec2(450.0, 450.0), Vec2::X, 16.0, 16.0).bbox(),
                ProjectFilter::BUILDING,
            )
            .count(),
        0
    );
}

#[test]
fn w0_water_cut_slot_rejects_count_before_payload() {
    use crate::map::TerrainChunkID;
    use common::saveload::Bincode;
    use std::collections::BTreeSet;

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
    let data = Bincode::encode(&BTreeSet::from([
        TerrainChunkID::new_i16(0, 0),
        TerrainChunkID::new_i16(1, 0),
    ]))
    .unwrap();
    let error = unsafe { &*std::ptr::addr_of!(crate::init::SAVELOAD_FUNCS) }
        .iter()
        .find(|entry| entry.name == "pending_construction_cuts")
        .unwrap();
    let error = (error.load)(&mut sim, &data[..1]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("too many construction cut chunks"),
        "{error}"
    );
}

#[test]
fn w0_water_direct_imports_reject_empty_configured_sea() {
    use crate::map::{procgen, WaterEdit};

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
    sim.edit_water(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    })
    .unwrap();
    assert!(!sim.map().environment().strict_water_siting());
    let before = sim.hashes();
    let rejected = Err(procgen::PresetImportError::WaterConfigured);
    assert_eq!(procgen::load_parismap(&mut sim.map_mut()), rejected);
    assert_eq!(
        procgen::load_testfield(&mut sim.map_mut(), Vec2::splat(64.0), 2, 32.0),
        rejected
    );
    assert_eq!(sim.hashes(), before);
}

#[test]
fn w0_water_indexed_links_share_the_topology_budget() {
    use crate::map::{LaneKind, LanePattern, MapProject, WaterEdit};
    use crate::world_command::{CommandOutcomes, Rejection};
    use crate::Replay;
    use geom::vec3;

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 0).unwrap();
    sim.edit_water(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    })
    .unwrap();
    sim.write::<Replay>().enabled = true;
    let pat = LanePattern {
        lanes_forward: vec![(LaneKind::Driving, 10.0); 16],
        lanes_backward: vec![(LaneKind::Driving, 10.0); 15],
    };
    let mut projects = Vec::new();
    let mut links = Vec::new();
    for index in 0..60 {
        let y = 40.0 + index as f32 * 14.0;
        projects.push(MapProject::ground(vec3(100.0, y, 10.0)));
        projects.push(MapProject::ground(vec3(220.0, y, 10.0)));
        links.push((index * 2, index * 2 + 1, None, pat.clone()));
    }
    sim.apply_commands([&WorldCommand::MapMakeMultipleConnections(projects, links)]);
    let outcomes = sim.read::<CommandOutcomes>();
    let results = outcomes.link_outcomes(0);
    let accepted = results
        .iter()
        .take_while(|(_, result)| result.is_ok())
        .count();
    assert!(accepted > 0);
    assert_eq!(
        results.last(),
        Some(&(
            accepted,
            Err(("MapMakeMultipleConnections", Rejection::WaterWorkLimit))
        )),
    );
    assert_eq!(sim.map().roads().len(), accepted);
    assert_eq!(sim.read::<Replay>().command_count(), 1);
}

#[test]
fn w0_water_rejects_alternating_terrain_links_before_profiling() {
    use crate::map::{Environment, LaneKind, LanePattern, Map, MapProject};
    use crate::world_command::{CommandOutcomes, Rejection};
    use geom::{vec2, AABB};

    init_test_simulation();
    let mut terrain = Environment::new(80, 1);
    terrain.terrain_apply(
        AABB::new_ll_ur(vec2(0.0, 0.0), vec2(40_959.0, 511.0)),
        |point| if (point.x as u32 / 16) % 2 == 0 { 12.0 } else { 0.0 },
    );
    terrain.set_sea_level(Some(-100)).unwrap();
    assert!(!terrain.strict_water_siting());

    let projects = [128.0, 384.0]
        .into_iter()
        .flat_map(|y| {
            [32.0, 40_928.0].map(|x| {
                let pos = vec2(x, y);
                MapProject::ground(pos.z(terrain.height(pos).unwrap()))
            })
        })
        .collect();
    let pattern = LanePattern {
        lanes_forward: vec![(LaneKind::Driving, 10.0)],
        lanes_backward: vec![],
    };
    let links = vec![(0, 1, None, pattern.clone()), (2, 3, None, pattern)];
    let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
    let mut map = Map::empty();
    assert!(map.install_environment(terrain));
    *sim.write::<Map>() = map;

    sim.apply_commands([&WorldCommand::MapMakeMultipleConnections(projects, links)]);
    let outcomes = sim.read::<CommandOutcomes>();
    let limit = Err(("MapMakeMultipleConnections", Rejection::WaterWorkLimit));
    assert_eq!(outcomes.link_outcomes(0), &[(0, limit), (1, limit)]);
    assert!(sim.map().roads().is_empty());
    assert!(sim.map().intersections().is_empty());
}

#[test]
fn w0_water_profile_samples_are_shared_by_indexed_links() {
    use crate::map::{LaneKind, LanePattern, MapProject, WaterEdit};
    use crate::world_command::{CommandOutcomes, Rejection};
    use geom::vec3;

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(20, 0).unwrap();
    sim.edit_water(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    })
    .unwrap();
    let pattern = LanePattern {
        lanes_forward: vec![(LaneKind::Driving, 10.0)],
        lanes_backward: vec![],
    };
    let projects = [(128.0, 9_000.0), (256.0, 9_000.0), (384.0, 100.0)]
        .into_iter()
        .flat_map(|(y, length)| [32.0, 32.0 + length].map(|x| MapProject::ground(vec3(x, y, 0.0))))
        .collect();
    let links = (0..3)
        .map(|i| (i * 2, i * 2 + 1, None, pattern.clone()))
        .collect();

    sim.apply_commands([&WorldCommand::MapMakeMultipleConnections(projects, links)]);
    assert_eq!(
        sim.read::<CommandOutcomes>().link_outcomes(0),
        &[
            (0, Ok(())),
            (1, Err(("MapMakeMultipleConnections", Rejection::WaterWorkLimit))),
            (2, Ok(())),
        ]
    );
    assert_eq!(sim.map().roads().len(), 2);
}

#[test]
fn w0_water_profile_samples_include_split_road_rebuilds() {
    use crate::map::{Environment, LaneKind, LanePattern, Map, MapProject, WaterEdit, ROAD_Z_OFFSET};
    use crate::world_command::{CommandOutcomes, Rejection};
    use geom::vec3;

    init_test_simulation();
    let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
    let mut terrain = Environment::new(80, 1);
    terrain.terrain_apply(terrain.bounds(), |_| 0.0);
    let mut map = Map::empty();
    assert!(map.install_environment(terrain));
    let pattern = LanePattern {
        lanes_forward: vec![(LaneKind::Driving, 10.0)],
        lanes_backward: vec![],
    };
    let (_, road) = map
        .make_connection(
            MapProject::ground(vec3(32.0, 256.0, 0.0)),
            MapProject::ground(vec3(40_928.0, 256.0, 0.0)),
            None,
            &pattern,
        )
        .unwrap();
    *sim.write::<Map>() = map;
    sim.edit_water(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    })
    .unwrap();
    let before = sim.hashes()["map"];

    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject {
            pos: vec3(20_480.0, 256.0, ROAD_Z_OFFSET),
            kind: road.into(),
        },
        to: MapProject::ground(vec3(20_480.0, 384.0, 0.0)),
        inter: None,
        pat: pattern,
    }]);
    assert_eq!(
        sim.read::<CommandOutcomes>().rejections_for(0),
        [("MapMakeConnection", Rejection::WaterWorkLimit)]
    );
    assert_eq!(sim.hashes()["map"], before);
}

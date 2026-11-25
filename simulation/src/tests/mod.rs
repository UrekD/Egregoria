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
use std::sync::{Mutex, Once};

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
mod p6_water;
mod p7_road_edit;
mod test_iso;
mod vehicles;
mod zone2_lot_layout;
mod zones2;

#[test]
fn w0_water_height_free_change_records_retry_without_mutation() {
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
    assert_eq!(sim.read::<Replay>().command_count(), initial_commands + 2);

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
    let locked =
        ZoneGrid::eligible_paint_cells_with_land(&map, &mixed, kind, Density::Low, |key| {
            key != (0, 0)
        });
    assert!(matches!(locked, Err(ZonePaintError::TileLocked((0, 0)))));
    assert!(matches!(
        ZoneGrid::eligible_paint_cells(&map, &wet, kind, Density::Low),
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
    let paint = ZoneGrid::eligible_paint_cells(&map, &mixed, kind, Density::Low).unwrap();
    let erase = ZoneGrid::eligible_paint_cells(&map, &mixed, None, Density::Low).unwrap();
    assert_eq!(paint.cells(), &[(2, 0), (2, 1), (3, 0), (3, 1)]);
    assert_eq!(erase.cells().len(), 8);
    drop(map);

    for (prepared, kind) in [(paint, kind), (erase, None)] {
        let vote = sim
            .read::<ZoneGrid>()
            .lot_zone_after_prepared_paint(&mixed, &prepared);
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
        previous_commands + 2
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

/// Run tests that replace the process-wide prototype registry in their own process.
pub(crate) fn in_prototype_fixture_process(module: &str, name: &str) -> bool {
    const FIXTURE: &str = "EGR_TEST_PROTOTYPE_FIXTURE";
    let (_, module) = module.split_once("::").unwrap();
    let test = format!("{module}::{name}");
    if std::env::var(FIXTURE).is_ok_and(|running| running == test) {
        return true;
    }

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test])
        .env(FIXTURE, &test)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("running 1 test"),
        "{test}\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

pub(crate) fn load_staged_prototypes() {
    prototypes::test_prototypes(concat!(
        "package.path = '../base_mod/?.lua'\nzones2_profiles_enabled = true\n",
        include_str!("../../../base_mod/data.lua")
    ));
    crate::init::register_simulation();
}

fn init_test_simulation() {
    static BASE_PROTOTYPES: Mutex<Option<&'static prototypes::Prototypes>> = Mutex::new(None);
    let mut base = BASE_PROTOTYPES.lock().unwrap();
    let base_is_loaded = base
        .zip(prototypes::try_prototypes())
        .is_some_and(|(base, current)| std::ptr::eq(base, current));
    if !base_is_loaded {
        // SAFETY: The mutex serializes default initialization, and custom
        // fixtures replace the registry only in their own subprocess.
        unsafe { crate::init::load_prototypes() };
        *base = Some(prototypes::prototypes());
    }
    crate::init::register_simulation();
}

#[test]
fn w3_water_test_context_reuses_base_prototypes() {
    let _first = TestCtx::new();
    let registry = prototypes::prototypes();
    let _second = TestCtx::new();
    assert!(
        std::ptr::eq(registry, prototypes::prototypes()),
        "creating another test context replaced the base prototype registry"
    );
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
fn w0_water_rejects_excess_lanes_before_topology_and_c1_keeps_the_inputs() {
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
    for (index, command) in [single, indexed].into_iter().enumerate() {
        sim.apply_commands([&command]);
        assert_eq!(
            sim.read::<CommandOutcomes>().last[0].1,
            Rejection::WaterWorkLimit
        );
        assert_eq!(sim.map().roads().len(), 0);
        assert_eq!(sim.read::<Replay>().command_count(), index + 1);
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

#[test]
fn zones2_mix_payload_rejections_leave_replay_and_paint_unchanged() {
    use crate::economy::Budget;
    use crate::map::{Density, ZoneCell, ZoneGrid, ZoneKind, ZonePaintError};
    use crate::world_command::{CommandOutcomes, Rejection, WorldCommands};
    use crate::{MapPreset, NewGameOptions, Replay};
    use common::saveload::{Bincode, JSON};
    use geom::OBB;

    if !in_prototype_fixture_process(
        module_path!(),
        "zones2_mix_payload_rejections_leave_replay_and_paint_unchanged",
    ) {
        return;
    }
    load_staged_prototypes();
    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        ..NewGameOptions::default()
    })
    .unwrap();
    for (index, kind) in ZoneKind::ALL.into_iter().enumerate() {
        let wire = Bincode::encode(&kind).unwrap();
        assert_eq!(wire, [index as u8]);
        assert_eq!(Bincode::decode::<ZoneKind>(&wire).unwrap(), kind);
        assert_eq!(
            JSON::decode::<ZoneKind>(&JSON::encode(&kind).unwrap()).unwrap(),
            kind
        );
    }
    for (index, density) in [Density::Low, Density::High, Density::Medium]
        .into_iter()
        .enumerate()
    {
        assert_eq!(Bincode::encode(&density).unwrap(), [index as u8]);
    }
    let mixed = JSON::decode(br#""MixedUse""#).expect("mixed-use wire kind");
    let area = OBB::new(Vec2::splat(160.0), Vec2::X, 16.0, 16.0);
    let paint = |area, kind, density| WorldCommand::ZonePaint {
        area,
        kind,
        density,
    };
    let low = paint(area, Some(mixed), Density::Low);
    let tax = WorldCommand::SetTaxRate {
        zone: mixed,
        rate_permille: 100,
    };
    let malformed = paint(OBB::ZERO, Some(mixed), Density::High);
    let invalid_rate = WorldCommand::SetTaxRate {
        zone: ZoneKind::Commercial,
        rate_permille: u16::MAX,
    };

    let unsupported = ZonePaintError::UnsupportedDensity {
        kind: mixed,
        density: Density::Low,
    };
    assert_eq!(
        ZoneGrid::eligible_paint_stroke(&sim.map(), &[area], Some(mixed), Density::Low),
        Err(unsupported),
    );
    assert_eq!(
        sim.write::<ZoneGrid>()
            .paint_on_map(&sim.map(), &area, Some(mixed), Density::Low),
        Err(unsupported),
    );

    let before = sim.read::<Replay>().command_count();
    let budget_before = Bincode::encode(&*sim.read::<Budget>()).unwrap();
    low.apply(&mut sim);
    assert_eq!(
        sim.read::<CommandOutcomes>().last,
        [(
            "ZonePaint",
            Rejection::UnsupportedDensity {
                kind: mixed,
                density: Density::Low
            }
        )],
    );

    let round_trip = |commands| -> WorldCommands {
        Bincode::decode(&Bincode::encode(&WorldCommands::from(commands)).unwrap()).unwrap()
    };
    let rejected = round_trip(vec![low, tax, malformed, invalid_rate]);
    sim.apply_commands(&rejected.commands);
    {
        let outcomes = sim.read::<CommandOutcomes>();
        assert!(outcomes.accepted.is_empty());
        assert_eq!(
            outcomes.last,
            [
                (
                    "ZonePaint",
                    Rejection::UnsupportedDensity {
                        kind: mixed,
                        density: Density::Low
                    }
                ),
                ("SetTaxRate", Rejection::UnsupportedTax(mixed)),
                ("ZonePaint", Rejection::InvalidZone),
                ("SetTaxRate", Rejection::InvalidTaxRate),
            ],
        );
    }
    assert_eq!(
        Bincode::encode(&*sim.read::<Budget>()).unwrap(),
        budget_before
    );
    assert_eq!(sim.read::<Replay>().command_count(), before);
    assert_eq!(sim.read::<ZoneGrid>().generation(), 0);
    assert!(sim.read::<ZoneGrid>().cells().is_empty());

    for density in [Density::Medium, Density::High] {
        let batch = round_trip(vec![paint(area, Some(mixed), density)]);
        sim.apply_commands(&batch.commands);
        assert_eq!(
            sim.read::<ZoneGrid>().lot_zone(&area),
            Some(ZoneCell {
                kind: mixed,
                density
            })
        );
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    }
    // A rejected-only stepped batch still advances the systems and the replay clock.
    let tick = sim.get_tick();
    sim.tick(&mut Simulation::schedule(), &rejected.commands);
    assert_eq!(sim.get_tick(), tick + 1);
    assert_eq!(sim.read::<Replay>().command_count(), before + 2);
    let replay = sim.read::<Replay>().clone();
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(
        replayed.read::<ZoneGrid>().cells(),
        sim.read::<ZoneGrid>().cells()
    );
    assert_eq!(replayed.read::<ZoneGrid>().generation(), 2);
    assert_eq!(replayed.get_tick(), sim.get_tick());
    assert_eq!(replayed.hashes(), sim.hashes());

    paint(area, None, Density::High).apply(&mut sim);
    assert!(sim.read::<ZoneGrid>().cells().is_empty());
    assert_eq!(sim.read::<Replay>().command_count(), before + 3);
}

#[test]
fn zones2_shipped_registry_rejects_inert_mixed_use_paint() {
    use crate::map::{Density, ZoneGrid, ZoneKind};
    use crate::world_command::CommandOutcomes;

    init_test_simulation();
    let mut sim = Simulation::new_game_with_replay(&crate::NewGameOptions {
        preset: crate::MapPreset::Flat,
        terrain_chunks: crate::NewGameOptions::MIN_TERRAIN_CHUNKS,
        ..crate::NewGameOptions::default()
    })
    .unwrap();
    let area = geom::OBB::new(Vec2::splat(160.0), Vec2::X, 16.0, 16.0);
    let paint = |kind| WorldCommand::ZonePaint {
        area,
        kind: Some(kind),
        density: Density::Medium,
    };
    paint(ZoneKind::Residential).apply(&mut sim);
    let grid_before = sim.read::<ZoneGrid>().cells().clone();
    let replay_before = sim.read::<crate::Replay>().command_count();
    paint(ZoneKind::MixedUse).apply(&mut sim);
    assert_eq!(sim.read::<ZoneGrid>().cells(), &grid_before);
    assert_eq!(sim.read::<crate::Replay>().command_count(), replay_before);
    assert!(sim.read::<CommandOutcomes>().accepted.is_empty());
}

#[test]
fn zones2_checked_placement_registers_one_home_and_preserves_a_rejected_parcel() {
    use crate::map::{Density, GrowableLotError, Lot, ZoneCell, ZoneKind};
    use crate::souls::{growth, residence::Residences};

    let mut test = TestCtx::new_fast();
    test.build_roads(&[Vec3::new(100.0, 100.0, 0.0), Vec3::new(400.0, 100.0, 0.0)]);
    let (lot, road) = {
        let map = test.g.map();
        let lot = map
            .lots()
            .values()
            .find(|lot| !Lot::footprint_obstructed(&map, lot.shape, false))
            .unwrap();
        (lot.id, lot.parent)
    };
    let mixed = ZoneCell {
        kind: ZoneKind::MixedUse,
        density: Density::Low,
    };
    let before = test.g.hashes();
    assert_eq!(
        test.g.place_zoned_growable(lot, mixed),
        Err(GrowableLotError::UnsupportedDensity)
    );
    assert_eq!(test.g.hashes(), before);

    let zone = ZoneCell {
        kind: ZoneKind::Residential,
        density: Density::Low,
    };
    let placed = test.g.place_zoned_growable(lot, zone).unwrap();
    assert!(test.g.map().get(lot).is_none());
    assert_eq!(
        test.g
            .map()
            .lot_origin(placed.building)
            .unwrap()
            .parent_road,
        road
    );
    assert!(test
        .g
        .read::<BuildingInfos>()
        .get(placed.building)
        .is_some());
    let homes = test.g.read::<Residences>();
    assert_eq!(homes.by_building.len(), 1);
    assert_eq!(
        homes.by_building[&placed.building].capacity,
        placed.housing_slots
    );
    assert_eq!(placed.job_slots, 0);
    drop(homes);

    let other = {
        let map = test.g.map();
        map.lots()
            .values()
            .find(|lot| lot.parent == road && !Lot::footprint_obstructed(&map, lot.shape, false))
            .unwrap()
            .id
    };
    let prepared = growth::plan_growable(&test.g, other, zone).unwrap();
    test.g.map_mut().remove_road(road).unwrap();
    let before = test.g.hashes();
    assert_eq!(
        test.g.commit_prepared_growable(prepared),
        Err(GrowableLotError::MissingLot)
    );
    assert_eq!(test.g.hashes(), before);
}

fn zones2_profiled_company() -> (TestCtx, BuildingID, crate::CompanyID) {
    use crate::map::{
        Density, GrowableBusinessState, GrowableDevelopment, GrowableSiteState,
        PreparedGrowableAppearance, ZoneCell, ZoneKind,
    };
    use crate::souls::{goods_company::company_soul, growth};
    use crate::{GrowablePlan, SoulID};

    let mut test = TestCtx::new_fast();
    test.build_roads(&[Vec3::new(100.0, 100.0, 0.0), Vec3::new(400.0, 100.0, 0.0)]);
    let zone = ZoneCell {
        kind: ZoneKind::Commercial,
        density: Density::Low,
    };
    let (plan, proto) = {
        let map = test.g.map();
        let site = map
            .lots()
            .values()
            .find_map(|lot| growth::check_growable_lot(&map, lot, zone).ok())
            .unwrap();
        let proto = site.company.unwrap().base.id;
        let door = site.shape.center().z(0.1);
        let plan = GrowablePlan::prepare(
            &map,
            site,
            Some(PreparedGrowableAppearance {
                development: GrowableDevelopment {
                    zone,
                    level: 1,
                    floors: 1,
                    roof_z: 4.0,
                    approved_value_band: 0,
                    home: None,
                    planned_jobs: 18,
                    site: GrowableSiteState::Active,
                    business: Some(GrowableBusinessState::AwaitingSpawn),
                    frozen_shell: None,
                },
                base_z: 0.0,
                door,
                mesh: Default::default(),
            }),
            None,
            18,
        )
        .unwrap();
        (plan, proto)
    };
    let placed = test.g.commit_prepared_growable(plan).unwrap();
    assert_eq!(placed.job_slots, 18);
    assert_eq!(
        test.g
            .growable_summary(placed.building)
            .unwrap()
            .active_job_slots,
        0
    );
    let SoulID::GoodsCompany(company) = company_soul(&mut test.g, placed.building, proto).unwrap()
    else {
        panic!("expected a company");
    };
    assert_eq!(test.g.get(company).unwrap().comp.max_workers, 18);
    assert!(company_soul(&mut test.g, placed.building, proto).is_none());
    (test, placed.building, company)
}

#[test]
fn zones2_profiled_business_slots_and_closure_are_authoritative() {
    use crate::economy::{Budget, BudgetCategory, Market};
    use crate::map::{GrowableBusinessState, PreparedGrowableAppearance, UpdateType};
    use crate::map_dynamic::{active_business, effective_load};
    use crate::souls::{
        desire::{Work, WorkKind},
        human::spawn_human,
        lifecycle,
    };
    use crate::SoulID;
    use prototypes::{GameTime, ItemID, TICKS_PER_HOUR};

    let (mut test, building, company) = zones2_profiled_company();
    let house = test.build_house_near(Vec2::new(250.0, 140.0));
    let worker = spawn_human(&mut test.g, house).unwrap();
    let mut work = Work::new(building, WorkKind::Worker, 0.0);
    work.employer = Some(company);
    test.g
        .world
        .edit_human(worker, |human| human.work = Some(work));
    test.g.world.companies[company].workers.0.push(worker);
    let seller = SoulID::GoodsCompany(company);
    let job = ItemID::new("job-opening");
    test.g.write::<Market>().produce(seller, job, -1);
    let door = test.g.map().get(building).unwrap().door_pos;
    test.g.write::<Market>().sell_all(seller, door.xy(), job, 0);
    let tax_category = BudgetCategory::Tax(crate::map::ZoneKind::Commercial);
    let accrue_hour = |sim: &mut Simulation| {
        let before = sim.read::<Budget>().total(tax_category);
        sim.write::<GameTime>().tick.0 += TICKS_PER_HOUR;
        let (world, resources) = sim.world_res();
        crate::economy::budget_update(world, resources);
        sim.read::<Budget>().total(tax_category) - before
    };
    let tax_level_one = accrue_hour(&mut test.g);
    assert!(tax_level_one > 0);

    let mut dirty = test.g.map().subscribe(UpdateType::Building);
    {
        let mut map = test.g.map_mut();
        let mut next = map.get(building).unwrap().development().unwrap().clone();
        next.level = 2;
        next.roof_z += 0.25;
        let appearance = |development| PreparedGrowableAppearance {
            development,
            base_z: 0.0,
            door,
            mesh: Default::default(),
        };
        let mut too_many = next.clone();
        too_many.planned_jobs = prototypes::MAX_PROFILED_SLOTS + 1;
        assert!(!map.update_growable_appearance(building, appearance(too_many)));
        assert_eq!(map.get(building).unwrap().development().unwrap().level, 1);
        assert!(map.update_growable_appearance(building, appearance(next)));
    }
    assert_eq!(dirty.take_updated_chunks().count(), 1);
    let upgraded = test.g.growable_summary(building).unwrap();
    assert_eq!(
        (
            upgraded.level,
            upgraded.planned_jobs,
            upgraded.active_job_slots
        ),
        (2, 18, 18)
    );
    assert_eq!(
        test.g.read::<BuildingInfos>().owner(building),
        Some(SoulID::GoodsCompany(company))
    );

    assert_eq!(upgraded.filled_jobs, 1);
    assert_eq!(test.g.get(company).unwrap().workers.0, [worker]);
    assert_eq!(
        test.g.get(worker).unwrap().work.as_ref().unwrap().employer,
        Some(company)
    );
    assert_eq!(test.g.read::<Market>().capital(seller, job), 17);
    assert_eq!(
        test.g.read::<Market>().inner()[&job]
            .sell_order(seller)
            .unwrap()
            .qty,
        17
    );
    let tax_level_two = accrue_hour(&mut test.g);
    assert!(tax_level_two > tax_level_one);
    assert!((tax_level_two - tax_level_one * 2).abs() <= 2);

    assert!(lifecycle::close_business(&mut test.g, company));
    assert!(!lifecycle::close_business(&mut test.g, company));
    let map = test.g.map();
    let site = map.get(building).unwrap();
    let active = active_business(site, &test.g.read::<BuildingInfos>(), test.g.world());
    assert!(active.is_none());
    assert_eq!(effective_load(site, None, active).water_demand(), 0.0);
    drop(map);
    let closed = test.g.growable_summary(building).unwrap();
    assert_eq!((closed.planned_jobs, closed.active_job_slots), (18, 0));
    assert!(matches!(
        closed.business,
        Some(GrowableBusinessState::ClosedUntil { .. })
    ));
}

#[test]
fn zones2_fix_rejects_mixed_and_unregistered_raw_companies() {
    use crate::map::{Building, BuildingKind, Map};
    use geom::{vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID};

    if !in_prototype_fixture_process(
        module_path!(),
        "zones2_fix_rejects_mixed_and_unregistered_raw_companies",
    ) {
        return;
    }
    load_staged_prototypes();
    let mut map = Map::empty();
    let obb = OBB::new(vec2(150.0, 150.0), Vec2::X, 20.0, 20.0);
    let gen = BuildingGen::CenteredDoor {
        vertical_factor: 1.0,
    };
    for name in ["mixed-medium-l1", "mixed-high-l1", "unregistered-company"] {
        let proto = GoodsCompanyID::new(name);
        assert_eq!(
            prototypes::try_prototype(proto).is_some(),
            name != "unregistered-company"
        );
        let kind = BuildingKind::GoodsCompany(proto);
        let special = map.build_special_building(&obb, kind, gen, None, None);
        let raw = Building::make(
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            map.environment(),
            obb,
            kind,
            gen,
            None,
            None,
        );
        assert_eq!((special, raw), (None, None), "{name}");
    }
    assert!(map.buildings().is_empty());
}

#[test]
fn zones2_fix_rejects_stale_dry_construction_grade() {
    use crate::map::{Density, GrowableLotError, TerraformKind, ZoneCell, ZoneKind};
    use crate::souls::growth;

    let mut test = TestCtx::new_fast();
    test.build_roads(&[Vec3::new(100.0, 100.0, 0.0), Vec3::new(400.0, 100.0, 0.0)]);
    let zone = ZoneCell {
        kind: ZoneKind::Residential,
        density: Density::Low,
    };
    let (lot, center, frontage, shape, grade) = {
        let map = test.g.map();
        let lot = map
            .lots()
            .values()
            .find(|lot| growth::check_growable_lot(&map, lot, zone).is_ok())
            .unwrap();
        let center = lot.shape.center();
        (
            lot.id,
            center,
            lot.parent,
            lot.shape,
            map.environment().height(center),
        )
    };
    let plan = growth::plan_growable(&test.g, lot, zone).unwrap();
    test.apply(&[WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center,
        radius: 24.0,
        amount: 200.0,
        level: 0.0,
        slope: None,
    }]);
    {
        let map = test.g.map();
        let live = map.get(lot).unwrap();
        assert_eq!(live.parent, frontage);
        assert_eq!(live.shape.corners, shape.corners);
        assert_ne!(map.environment().height(center), grade);
        assert!(growth::check_growable_lot(&map, live, zone).is_ok());
    }
    let before = test.g.hashes();
    assert_eq!(
        test.g.commit_prepared_growable(plan),
        Err(GrowableLotError::InfeasibleGrade)
    );
    assert_eq!(test.g.hashes(), before);
    assert!(test.g.place_zoned_growable(lot, zone).is_ok());
}

#[test]
fn zones2_fix_closure_survives_a_snapshot_before_the_drain() {
    use crate::economy::Market;
    use crate::map::GrowableBusinessState;
    use crate::souls::{goods_company::company_soul, lifecycle};
    use crate::SoulID;
    use common::saveload::{SlotId, SnapshotRevision};
    use prototypes::{GameTime, ItemID};

    let (mut test, building, company) = zones2_profiled_company();
    let proto = test.g.get(company).unwrap().comp.proto;
    assert!(lifecycle::close_business(&mut test.g, company));
    assert!(test.g.get(company).is_some());

    let root = std::env::temp_dir().join(format!("egr-zones2-closure-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let slot = SlotId::new("zones2-closure").unwrap();
    test.g
        .save_to_slot(&root, &slot, SnapshotRevision::new(1))
        .unwrap();
    let mut loaded = Simulation::load_from_slot(&root, &slot).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(loaded.hashes(), test.g.hashes());

    test.g.apply_commands([]);
    loaded.apply_commands([]);
    assert!(loaded.get(company).is_none());
    assert_eq!(loaded.read::<BuildingInfos>().owner(building), None);
    assert_eq!(
        loaded
            .read::<Market>()
            .capital(SoulID::GoodsCompany(company), ItemID::new("job-opening")),
        0
    );
    assert_eq!(loaded.hashes(), test.g.hashes());
    assert!(company_soul(&mut loaded, building, proto).is_none());

    let Some(GrowableBusinessState::ClosedUntil { retry_elapsed_tick }) =
        loaded.growable_summary(building).unwrap().business
    else {
        panic!("expected closed site");
    };
    loaded.write::<GameTime>().tick.0 += retry_elapsed_tick;
    let Some(SoulID::GoodsCompany(reopened)) = company_soul(&mut loaded, building, proto) else {
        panic!("closed site did not reopen");
    };
    assert_ne!(reopened, company);
}

fn zones2_mixed_role_fixture() -> (TestCtx, BuildingID, crate::CompanyID) {
    use crate::map::{Density, ZoneCell, ZoneKind};
    use crate::souls::residence::{self, Residence, Residences};
    use common::saveload::Bincode;

    let (test, building, company) = zones2_profiled_company();
    // Exercise the private mixed-role transition with a registered stand-in seller.
    let home = residence::prototype(Density::Medium, 1).unwrap();
    let mut map = test.g.map_mut();
    let site = map.get(building).unwrap();
    let mut development = site.development().unwrap().clone();
    development.zone = ZoneCell {
        kind: ZoneKind::MixedUse,
        density: Density::Medium,
    };
    development.home = Some(home.id);
    development.planned_jobs = 12;
    let profile = Bincode::encode(site.development().unwrap()).unwrap();
    let mut record = Bincode::encode(site).unwrap();
    assert!(record.ends_with(&profile));
    record.truncate(record.len() - profile.len());
    record.extend(Bincode::encode(&development).unwrap());
    map.buildings[building] = Bincode::decode(&record).unwrap();
    drop(map);
    test.g
        .write::<Residences>()
        .insert(building, Residence::from(home));
    (test, building, company)
}

#[test]
fn zones2_fix_hiring_does_not_reclassify_a_home_trip() {
    use crate::economy::TripPurpose;
    use crate::map_dynamic::Destination;
    use crate::souls::{goods_company::company_system, human::spawn_human, lifecycle};
    use crate::transportation::Location;
    use crate::{FreightStationEnt, HumanEnt, HumanWork, ParCommandBuffer};

    let (mut test, building, company) = zones2_mixed_role_fixture();
    let tenant = spawn_human(&mut test.g, building).unwrap();
    let coworker_home = test.build_house_near(Vec2::new(250.0, 140.0));
    let commuter = spawn_human(&mut test.g, coworker_home).unwrap();
    let target = Some(Destination::Building(building));
    let (world, resources) = test.g.world_res();
    world.edit_human(tenant, |human| {
        human.location = Location::Outside;
        human.decision.kind = human.go_home();
    });

    // The work assignment arrives after the Home action was queued at this address.
    world.companies[company].workers.0.extend([tenant, commuter]);
    company_system(world, resources);
    let freight = resources.read::<ParCommandBuffer<FreightStationEnt>>();
    let humans = resources.read::<ParCommandBuffer<HumanEnt>>();
    let infos = resources.read::<BuildingInfos>();
    let map = resources.read::<crate::Map>();

    for id in [tenant, commuter] {
        world.edit_human(id, |human| {
            let work = human.work.as_mut().unwrap();
            assert_eq!(work.employer, Some(company));
            if id == commuter {
                human.decision.kind = work.apply(&human.location, &human.router);
            }
            assert!(!human.decision.kind.update(
                &mut human.router,
                &infos,
                &map,
                &freight,
                &humans,
                id,
                Some(work),
            ));
            assert_eq!(human.router.target_dest, target);
        });
    }
    drop((freight, humans, infos, map));
    let mut route_changes = Vec::new();
    world.collect_active(HumanWork::RouteChanged, &mut route_changes);
    assert_eq!(route_changes, [tenant, commuter]);
    assert_eq!(
        test.g.get(tenant).unwrap().router.trip_purpose,
        TripPurpose::Home
    );
    assert_eq!(
        test.g.get(commuter).unwrap().router.trip_purpose,
        TripPurpose::Work
    );

    assert!(lifecycle::close_business(&mut test.g, company));
    test.g.apply_commands([]);
    assert!(test.g.get(company).is_none());
    assert_eq!(
        test.g
            .read::<BuildingInfos>()
            .residence_of(crate::SoulID::Human(tenant)),
        Some(building)
    );
    test.g
        .world
        .collect_active(HumanWork::RouteChanged, &mut route_changes);
    assert_eq!(route_changes, [tenant]);
    let resident = test.g.get(tenant).unwrap();
    assert!(resident.work.is_none());
    assert_eq!(resident.router.trip_purpose, TripPurpose::Home);
    assert_eq!(resident.router.target_dest, target);
    let commuter = test.g.get(commuter).unwrap();
    assert!(commuter.work.is_none());
    assert_eq!(commuter.router.target_dest, None);
}

#[test]
fn zones2_fix_mixed_upgrade_keeps_twelve_job_slots() {
    use crate::map::PreparedGrowableAppearance;

    let (test, building, _) = zones2_mixed_role_fixture();
    let mut map = test.g.map_mut();
    let site = map.get(building).unwrap();
    let mut next = site.development().unwrap().clone();
    next.level = 2;
    next.roof_z += 1.0;
    let (base_z, door) = (site.height, site.door_pos);
    let appearance = |development| PreparedGrowableAppearance {
        development,
        base_z,
        door,
        mesh: Default::default(),
    };
    let mut invalid = next.clone();
    invalid.planned_jobs = 13;
    assert!(!map.update_growable_appearance(building, appearance(invalid)));
    assert!(map.update_growable_appearance(building, appearance(next)));
    assert_eq!(
        map.get(building)
            .unwrap()
            .development()
            .unwrap()
            .planned_jobs,
        12
    );
}

#[test]
fn zones2_fix_scrubs_departing_company_trades_at_the_drain() {
    use crate::economy::{Trade, TradeTarget};
    use crate::map::BuildingKind;
    use crate::souls::{goods_company::company_soul, growth, lifecycle};
    use crate::{Entity, SoulID};
    use prototypes::{GoodsCompanyID, ItemID, Money};

    let (mut test, _, first) = zones2_profiled_company();
    let kind = BuildingKind::GoodsCompany(GoodsCompanyID::new("commercial-l1"));
    let mut add_company = || {
        let lot = test.g.map().lots().keys().next().unwrap();
        let building = test.g.map_mut().place_growable(lot, kind).unwrap();
        growth::on_growable_placed(&test.g, building, kind);
        let Some(SoulID::GoodsCompany(company)) =
            company_soul(&mut test.g, building, kind.as_goods_company().unwrap())
        else {
            panic!("company did not start");
        };
        company
    };
    let second = add_company();
    let remaining = add_company();
    let item = ItemID::new("retail-goods");
    let trade = |buyer, seller| Trade {
        buyer: TradeTarget(SoulID::GoodsCompany(buyer)),
        seller: TradeTarget(SoulID::GoodsCompany(seller)),
        qty: 1,
        kind: item,
        money_delta: Money::ZERO,
    };
    let survivor = &mut test.g.world.companies[remaining];
    survivor.sold.0 = vec![trade(first, remaining), trade(second, remaining)];
    survivor.bought.0.insert(
        item,
        vec![
            trade(remaining, first),
            trade(remaining, second),
            trade(remaining, remaining),
        ],
    );

    // Call the same ordered drop hooks as the command buffer and observe their shared drain.
    for id in [first, second] {
        let entity = crate::CompanyEnt::storage_mut(&mut test.g.world)
            .remove(id)
            .unwrap();
        lifecycle::company_dropped(&mut test.g, id, entity);
    }
    assert_eq!(test.g.get(remaining).unwrap().sold.0.len(), 2);
    assert_eq!(test.g.get(remaining).unwrap().bought.0[&item].len(), 3);
    test.g.apply_commands([]);
    let survivor = test.g.get(remaining).unwrap();
    assert!(survivor.sold.0.is_empty());
    assert_eq!(survivor.bought.0[&item].len(), 1);
    assert_eq!(
        survivor.bought.0[&item][0].seller.0,
        SoulID::GoodsCompany(remaining)
    );
}

mod land_ownership;

#[cfg(test)]
mod tiles_fix_regressions {
    use super::init_test_simulation;
    use crate::map::{
        BuildingKind, LandMode, LandTileId, LanePatternBuilder, Map, MapProject, WaterEdit,
        Zone, ZoneGrid, ZoneKind,
    };
    use crate::world_command::{CommandOutcomes, Rejection};
    use crate::{
        MapPreset, NewGameOptions, Replay, Simulation, SlotId, SnapshotRevision, WorldCommand,
    };
    use common::saveload::{Bincode, Encoder, JSON};
    use geom::{vec2, vec3, Polygon, Radians, Vec2, OBB};
    use prototypes::{BuildingGen, FreightStationPrototypeID};

    fn game(land_mode: LandMode) -> Simulation {
        init_test_simulation();
        Simulation::new_game_with_replay(&NewGameOptions {
            preset: MapPreset::Flat,
            terrain_chunks: 16,
            land_mode,
            ..NewGameOptions::default()
        })
        .unwrap()
    }

    #[test]
    fn tiles_fix_protects_live_resources() {
        let mut sim = game(LandMode::Tiled);
        assert!(sim.try_write::<Map>().is_none());
        assert!(sim.try_write::<ZoneGrid>().is_none());
        assert!(sim.unrestricted_world_res().is_none());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sim.insert(Map::default());
        }))
        .is_err());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(sim.write_or_default::<ZoneGrid>());
        }))
        .is_err());
        assert!(game(LandMode::Unrestricted).try_write::<Map>().is_some());
    }

    #[test]
    fn tiles_fix_admits_unrestricted_commands_individually() {
        let mut sim = game(LandMode::Unrestricted);
        let opening = sim.read::<Replay>().command_count();
        let mut pat = LanePatternBuilder::default().build();
        pat.lanes_forward.resize(17, pat.lanes_forward[0]);
        pat.lanes_backward.resize(16, pat.lanes_forward[0]);
        let road = WorldCommand::MapMakeConnection {
            from: MapProject::ground(vec3(500., 500., 0.)),
            to: MapProject::ground(vec3(600., 500., 0.)),
            inter: None,
            pat,
        };
        let water = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
            level_dm: Some(-100),
        });
        let tax = WorldCommand::SetTaxRate {
            zone: ZoneKind::Residential,
            rate_permille: 200,
        };
        sim.apply_commands([&water, &road, &tax]);
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(
            outcomes.rejections_for(1),
            [("MapMakeConnection", Rejection::WaterWorkLimit)]
        );
        assert!(outcomes.accepted.contains(&2));
        assert_eq!(sim.read::<Replay>().command_count(), opening + 3);
    }

    #[test]
    fn tiles_fix_reports_excess_purchase_indices() {
        let mut sim = game(LandMode::Tiled);
        let buy = WorldCommand::BuyTile {
            tile: LandTileId { x: 9, y: 7 },
        };
        sim.apply_commands(std::iter::repeat_n(&buy, 258));
        let outcomes = sim.read::<CommandOutcomes>();
        assert!(outcomes.accepted.is_empty());
        assert_eq!(outcomes.tile_purchase(257), outcomes.tile_purchase(256));
        assert!(outcomes.tile_purchase(257).unwrap().is_err());
        assert_eq!(outcomes.tile_purchase(258), None);
    }

    #[test]
    fn tiles_fix_preview_rejects_occupied_special_site() {
        let mut sim = game(LandMode::Unrestricted);
        let build = WorldCommand::MapBuildSpecialBuilding {
            pos: OBB::new(vec2(1600., 1600.), Vec2::X, 60., 40.),
            kind: BuildingKind::RailFreightStation(FreightStationPrototypeID::new(
                "freight-station",
            )),
            gen: BuildingGen::NoWalkway {
                door_pos: Vec2::ZERO,
            },
            zone: None,
            connected_road: None,
        };
        assert!(build.preview(&sim).is_ok());
        sim.apply_commands([&build]);
        assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
        let rejection = build.preview(&sim).unwrap_err();
        sim.apply_commands([&build]);
        assert_eq!(
            sim.read::<CommandOutcomes>().rejections_for(0),
            [("MapBuildSpecialBuilding", rejection)]
        );
    }

    #[test]
    fn tiles_fix_bounds_nested_command_decode() {
        let commands = [
            WorldCommand::SpawnTrain {
                wagons: vec![prototypes::RollingStockID::new("bogus"); 257],
                lane: Default::default(),
                dist: 0.,
            },
            WorldCommand::TransitCreateLine {
                stops: vec![Default::default(); 31],
            },
        ];
        for command in commands {
            assert!(Bincode::decode::<WorldCommand>(&Bincode::encode(&command).unwrap()).is_err());
        }
    }

    fn zone_with_vertices(count: usize) -> Zone {
        let center = vec2(700.0, 700.0);
        let vertices = (0..count)
            .map(|index| {
                let angle = Radians(std::f32::consts::TAU * index as f32 / count as f32);
                center + Vec2::from_angle(angle) * 20.0
            })
            .collect();
        Zone::new(Polygon(vertices), Vec2::X)
    }

    #[test]
    fn tiles_fix_bounds_zone_command_decode() {
        for count in [256, 257] {
            let zone = zone_with_vertices(count);
            let commands = [
                WorldCommand::UpdateZone {
                    building: Default::default(),
                    zone: zone.clone(),
                },
                WorldCommand::MapBuildSpecialBuilding {
                    pos: OBB::new(vec2(1600.0, 1600.0), Vec2::X, 60.0, 40.0),
                    kind: BuildingKind::RailFreightStation(FreightStationPrototypeID::new(
                        "freight-station",
                    )),
                    gen: BuildingGen::NoWalkway {
                        door_pos: Vec2::ZERO,
                    },
                    zone: Some(zone),
                    connected_road: None,
                },
            ];
            for command in commands {
                let bytes = Bincode::encode(&command).unwrap();
                assert_eq!(Bincode::decode::<WorldCommand>(&bytes).is_ok(), count == 256);
                let json = JSON::encode(&command).unwrap();
                assert_eq!(JSON::decode::<WorldCommand>(&json).is_ok(), count == 256);
                assert_eq!(command.wire_error().is_none(), count == 256);
            }
        }
    }

    #[test]
    fn tiles_fix_reloads_unrestricted_zone_with_257_vertices() {
        init_test_simulation();
        let sim = Simulation::new_authored_flat(5, 0).unwrap();
        let zone = zone_with_vertices(257);
        let building = {
            let mut map = sim.write::<Map>();
            let building = map
                .build_special_building(
                    &OBB::new(vec2(200.0, 200.0), Vec2::X, 16.0, 16.0),
                    BuildingKind::ExternalTrading,
                    BuildingGen::NoWalkway {
                        door_pos: Vec2::ZERO,
                    },
                    Some(Zone::new(
                        Polygon::centered_rect(vec2(700.0, 700.0), 32.0, 32.0),
                        Vec2::X,
                    )),
                    None,
                )
                .unwrap();
            map.update_zone(building, zone.clone()).unwrap();
            building
        };
        let root = std::env::temp_dir().join(format!("egregoria-tiles-zone-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let slot = SlotId::new("tiles_zone").unwrap();
        sim.save_to_slot(&root, &slot, SnapshotRevision::new(1))
            .unwrap();
        let loaded = Simulation::load_from_slot(&root, &slot);
        std::fs::remove_dir_all(root).unwrap();
        let loaded = loaded.unwrap();
        let map = loaded.map();
        let loaded_zone = map.get(building).unwrap().zone.as_ref().unwrap();
        assert_eq!(loaded_zone.poly.as_slice(), zone.poly.as_slice());
        assert_eq!(loaded_zone.area, zone.area);
        assert_eq!(loaded_zone.filldir, zone.filldir);
    }

    #[test]
    fn tiles_fix_removes_a_building_when_its_locked_cut_is_masked() {
        use crate::map::{LandOwnership, OwnedTileRect, TileExtent};

        init_test_simulation();
        let sim = Simulation::new_authored_flat(4, 0).unwrap();
        let mut map = sim.map_mut();
        let gen = BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        };
        let place = |map: &mut Map, x, size| {
            map.build_special_building(
                &OBB::new(vec2(x, 736.), Vec2::X, size, size),
                BuildingKind::ExternalTrading,
                gen,
                None,
                None,
            )
            .unwrap()
        };
        let first = place(&mut map, 995., 10.);
        let second = place(&mut map, 1011., 20.);
        map.install_land_ownership(
            LandOwnership::tiled(
                TileExtent {
                    width: 4,
                    height: 4,
                },
                OwnedTileRect {
                    origin: LandTileId { x: 1, y: 1 },
                    width: 1,
                    height: 1,
                },
                Default::default(),
            )
            .unwrap(),
        );
        assert!(map.remove_building(first).is_some());
        assert!(map.remove_building(second).is_none());
    }

    #[test]
    fn tiles_fix_indexed_split_keeps_local_lot_parents_live() {
        let mut sim = game(LandMode::Tiled);
        let (original, points) = {
            let map = sim.map();
            let road = map
                .roads()
                .values()
                .find(|road| road.has_sidewalks() && road.length() > 500.)
                .unwrap();
            (
                road.id,
                [0.35, 0.72].map(|t| road.points().point_along(road.length() * t)),
            )
        };
        let projects: Vec<_> = points
            .into_iter()
            .flat_map(|pos| {
                [
                    MapProject {
                        pos,
                        kind: original.into(),
                    },
                    MapProject::ground(pos + vec3(0., -125., 0.)),
                ]
            })
            .collect();
        let pat = LanePatternBuilder::default().build();
        let command = WorldCommand::MapMakeMultipleConnections(
            projects,
            vec![(0, 1, None, pat.clone()), (2, 3, None, pat)],
        );
        sim.apply_commands([&command]);
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(outcomes.accepted, [0], "{:?}", outcomes.last);
        assert_eq!(outcomes.link_outcomes(0).len(), 2);
        let map = sim.map();
        assert!(map.get(original).is_none());
        assert!(!map.lots().is_empty());
        assert!(map.lots().values().all(|lot| map.get(lot.parent).is_some()));
        drop(map);
        drop(outcomes);
        let replay = sim.read::<Replay>().clone();
        let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
        assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
        assert_eq!(sim.hashes(), replayed.hashes());
    }

    #[test]
    fn tiles_fix_train_tail_must_fit_on_owned_rail() {
        let mut sim = game(LandMode::Tiled);
        let road = sim
            .map_mut()
            .make_connection(
                MapProject::ground(vec3(3800., 3940., 0.)),
                MapProject::ground(vec3(4280., 3940., 0.)),
                None,
                &LanePatternBuilder::new().rail(true).build(),
            )
            .unwrap()
            .1;
        let lane = sim
            .map()
            .get(road)
            .unwrap()
            .lanes_iter()
            .find(|(_, kind)| kind.is_rail())
            .unwrap()
            .0;
        let spawn = WorldCommand::SpawnTrain {
            wagons: vec![
                prototypes::RollingStockID::new("locomotive"),
                prototypes::RollingStockID::new("freight-wagon"),
            ],
            lane,
            dist: 5.0,
        };
        assert_eq!(spawn.preview(&sim).unwrap_err(), Rejection::InvalidTrain);
        sim.apply_commands([&spawn]);
        assert_eq!(
            sim.read::<CommandOutcomes>().rejections_for(0),
            [("SpawnTrain", Rejection::InvalidTrain)]
        );
        assert!(sim.world().trains.is_empty());
    }

    #[test]
    fn tiles_fix_frontage_preparation_charges_candidate_work() {
        let sim = game(LandMode::Tiled);
        let map = sim.map();
        let road = map
            .roads()
            .values()
            .find(|road| road.has_sidewalks() && road.length() < 500.)
            .unwrap();
        assert!(map.check_land_frontage_capacity([road], &mut 256).is_err());
    }

    #[test]
    fn tiles_fix_reloads_pre_tiles_slot_with_oversized_zone_command() {
        use crate::utils::replay::ReplayJson;
        use crate::{SimulationDeser, SimulationSer, SlotSnapshotHeader, SLOT_SNAPSHOT_MAGIC};
        use common::saveload::{slot_paths, CompressedBincode};
        use prototypes::Tick;

        let mut sim = game(LandMode::Unrestricted);
        let zone = zone_with_vertices(257);
        let command = WorldCommand::UpdateZone {
            building: Default::default(),
            zone: zone.clone(),
        };
        let tick = Tick(sim.get_tick());
        let start = sim.read::<Replay>().command_count();
        sim.apply_commands([&command]);
        assert_eq!(sim.read::<Replay>().command_count(), start);
        {
            // The pre-TILES dispatcher recorded the command before checking the building ID.
            let mut replay = sim.write::<Replay>();
            replay.push(tick, command);
            replay.record_batch(start, false, tick);
        }
        let bytes = Bincode::encode(&sim).unwrap();
        let mut legacy: SimulationDeser = Bincode::decode(&bytes).unwrap();
        legacy.res.remove("land_ownership").unwrap();
        let opts = *sim.read::<crate::SimulationOptions>();
        legacy.res.insert(
            "simoptions".into(),
            Bincode::encode(&(
                opts.terrain_size,
                opts.save_replay,
                opts.preset,
                opts.seed,
                opts.starting_money_bucks,
                opts.progression,
            ))
            .unwrap(),
        );
        let revision = SnapshotRevision::new(1);
        let bytes = CompressedBincode::encode(&(
            SlotSnapshotHeader {
                magic: SLOT_SNAPSHOT_MAGIC,
                version: legacy._header.version.clone(),
                revision,
            },
            SimulationSer {
                header: legacy._header,
                world: &legacy.world,
                res: legacy.res,
            },
        ))
        .unwrap();
        let root =
            std::env::temp_dir().join(format!("egregoria-tiles-legacy-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let slot = SlotId::new("tiles_legacy").unwrap();
        let paths = slot_paths(&root, &slot).unwrap();
        std::fs::write(paths.zip, bytes).unwrap();
        std::fs::write(
            paths.replay,
            ReplayJson::encode_sidecar(&sim.read::<Replay>(), revision).unwrap(),
        )
        .unwrap();
        let loaded = Simulation::load_from_slot(&root, &slot);
        let sidecar = Simulation::load_replay_from_slot(&root, &slot);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            loaded.is_ok() && sidecar.is_ok(),
            "snapshot: {:?}, sidecar: {:?}",
            loaded.as_ref().err(),
            sidecar.as_ref().err()
        );
        let loaded = loaded.unwrap();
        assert_eq!(sim.hashes(), loaded.hashes());
        assert!(!loaded.map().land_ownership().is_tiled());
        let sidecar = sidecar.unwrap();
        assert_eq!(sidecar.command_count(), start + 1);
        let Some(WorldCommand::UpdateZone {
            zone: loaded_zone, ..
        }) = sidecar.commands(start..start + 1).next()
        else {
            panic!("legacy zone command missing from the sidecar");
        };
        assert_eq!(loaded_zone.poly.as_slice(), zone.poly.as_slice());
    }

    #[test]
    fn tiles_fix_rejects_outside_protected_deck() {
        let sim = game(LandMode::Tiled);
        let road = sim
            .map()
            .land_ownership()
            .reserves()
            .find_map(|root| match root {
                crate::map::ProtectedRoot::Road(crate::map::GatewayRole::Highway, id) => Some(id),
                _ => None,
            })
            .unwrap();
        sim.map_mut().roads[road].width = 10_000.0;
        assert!(crate::map::validate_loaded_land(&sim).is_err());
    }

    #[test]
    fn tiles_fix_requires_external_and_local_freight_on_the_protected_rail() {
        let sim = game(LandMode::Tiled);
        let (id, station) = {
            let map = sim.map();
            let external = map.external_train_stations[0];
            let local = map
                .buildings()
                .values()
                .find(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))
                .unwrap();
            (external, local.kind)
        };
        sim.map_mut().buildings[id].kind = station;
        assert!(crate::map::validate_loaded_land(&sim).is_err());
    }

    #[test]
    fn tiles_fix_wire_invalid_edit_cannot_poison_snapshot_or_sidecar() {
        let mut sim = game(LandMode::Unrestricted);
        let malformed = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
            level_dm: Some(i16::MIN),
        });
        let tax = WorldCommand::SetTaxRate {
            zone: ZoneKind::Residential,
            rate_permille: 200,
        };
        sim.apply_commands([&malformed, &tax]);
        let missing = WorldCommand::EditWater(WaterEdit::RemoveFreshWater {
            body: crate::map::WaterBodyId(1),
        });
        let sea = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
            level_dm: Some(-100),
        });
        let raise = WorldCommand::Terraform {
            kind: crate::map::TerraformKind::Elevation,
            center: vec2(7072., 3072.),
            radius: 48.,
            amount: 100.,
            level: 0.,
            slope: None,
        };
        let opening = sim.read::<Replay>().command_count();
        sim.apply_commands([&missing]);
        assert!(!sim.read::<CommandOutcomes>().rejections_for(0).is_empty());
        sim.apply_commands([&sea, &raise]);
        assert_eq!(
            sim.read::<CommandOutcomes>().rejections_for(1),
            [("Terraform", Rejection::WaterWorkLimit)]
        );
        assert_eq!(sim.read::<Replay>().command_count(), opening + 3);
        let root = std::env::temp_dir().join(format!("egregoria-tiles-fix-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let slot = SlotId::new("tiles_fix").unwrap();
        sim.save_to_slot(&root, &slot, SnapshotRevision::new(1))
            .unwrap();
        let snapshot = Simulation::load_from_slot(&root, &slot);
        let sidecar = Simulation::load_replay_from_slot(&root, &slot);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(sim.hashes(), snapshot.unwrap().hashes());
        let (mut replayed, mut loader) = Simulation::try_from_replay(sidecar.unwrap()).unwrap();
        assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
        assert_eq!(sim.hashes(), replayed.hashes());
    }
}

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
    let initial_revision = sim.map().environment.water_revision();
    let initial_commands = sim.read::<Replay>().command_count();
    let edit = WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    };

    let outcome = sim.edit_water(edit.clone()).unwrap();
    assert!(outcome.changed);
    assert!(outcome.dirty.terrain().is_empty());
    assert_eq!(sim.map().environment.sea_level_dm(), Some(-100));
    assert_eq!(sim.map().environment.water_revision(), initial_revision + 1);
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
    assert_eq!(sim.map().environment.water_revision(), initial_revision + 1);
    assert_eq!(sim.read::<Replay>().command_count(), initial_commands + 1);

    let replay = sim.read::<Replay>().clone();
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert!(loader.advance_tick(&mut replayed, &mut Simulation::schedule()));
    assert_eq!(replayed.map().environment.sea_level_dm(), Some(-100));
    assert_eq!(
        replayed.map().environment.water_revision(),
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
    assert_eq!(ctx.g.map().environment.sea_level_dm(), None);
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
    map.environment = Environment::new(5, 3);
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

    map.environment = Environment::default();
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

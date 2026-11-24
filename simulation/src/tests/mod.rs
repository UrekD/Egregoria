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
    let edit = WaterEdit::SetSeaLevel { level_dm: Some(-100) };

    let outcome = sim.edit_water(edit.clone()).unwrap();
    assert!(outcome.changed);
    assert!(outcome.dirty.terrain().is_empty());
    assert_eq!(sim.map().environment.sea_level_dm(), Some(-100));
    assert_eq!(sim.map().environment.water_revision(), initial_revision + 1);
    {
        let replay = sim.read::<Replay>();
        assert_eq!(replay.command_count(), initial_commands + 1);
        assert!(matches!(
            replay.commands(initial_commands..initial_commands + 1).next(),
            Some(WorldCommand::EditWater(WaterEdit::SetSeaLevel { level_dm: Some(-100) }))
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
    assert_eq!(replayed.map().environment.water_revision(), initial_revision + 1);
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

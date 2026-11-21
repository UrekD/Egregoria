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

mod test_iso;
mod vehicles;

fn init_test_simulation() {
    static INIT: Once = Once::new();
    INIT.call_once(crate::init::init);
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
        b
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
    let regular = TestCtx::new();
    let regular_tick = regular.g.get_tick();
    let mut fast = TestCtx::new_fast();
    let fast_tick = fast.g.get_tick();

    // A tick can run without this resource, but serializing the simulation requires it.
    let (_, resources) = fast.g.world_res();
    assert!(resources.remove::<SimulationOptions>().is_some());

    fast.tick_fast();
    fast.tick_fast();

    assert_eq!(fast.g.get_tick(), fast_tick + 2);
    assert_eq!(regular.g.get_tick(), regular_tick);
}

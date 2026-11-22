use super::TestCtx;
use crate::map::{
    LanePatternBuilder, LotLayout, Map, MapProject, ProjectFilter, RoadID, ZoneGrid, ZoneKind,
    UNIFORM_LOT_SIZE,
};
use crate::utils::replay::ReplayJson;
use crate::world_command::{CommandOutcomes, WorldCommand};
use crate::{MapPreset, NewGameOptions, Replay, Simulation, SimulationDeser, SimulationOptions};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, Vec2, Vec3, OBB};
use prototypes::Density;
use std::collections::BTreeSet;

fn connect(from: Vec3, to: Vec3) -> WorldCommand {
    WorldCommand::MapMakeConnection {
        from: MapProject::ground(from),
        to: MapProject::ground(to),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }
}

fn street() -> WorldCommand {
    connect(vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0))
}

fn recorded_sim() -> Simulation {
    drop(TestCtx::new());
    Simulation::new_with_options(SimulationOptions {
        terrain_size: NewGameOptions::MIN_TERRAIN_CHUNKS,
        preset: MapPreset::Flat,
        ..SimulationOptions::LEGACY
    })
}

fn road_with_lots(map: &Map) -> RoadID {
    map.lots().values().next().expect("street has lots").parent
}

fn sides(map: &Map, road: RoadID) -> Vec<(f32, f32)> {
    map.lots()
        .values()
        .filter(|lot| lot.parent == road)
        .map(|lot| {
            let [width, depth] = lot.shape.axis();
            (width.mag(), depth.mag())
        })
        .collect()
}

fn distinct_sizes(map: &Map, road: RoadID) -> BTreeSet<u32> {
    sides(map, road)
        .into_iter()
        .map(|(width, _)| width.round() as u32)
        .collect()
}

fn assert_uniform(map: &Map, road: RoadID) {
    let sides = sides(map, road);
    assert!(sides.len() >= 12, "only {} lots on the street", sides.len());
    for (width, depth) in sides {
        assert!(
            (width - UNIFORM_LOT_SIZE).abs() < 0.01 && (depth - UNIFORM_LOT_SIZE).abs() < 0.01,
            "{width} x {depth} lot in the uniform layout"
        );
    }
}

fn assert_accepted(sim: &Simulation) {
    assert!(sim.read::<CommandOutcomes>().last.is_empty());
}

fn replay_matches(sim: &Simulation) -> Simulation {
    let bytes = ReplayJson::encode(&*sim.read::<Replay>()).unwrap();
    let (mut replayed, mut loader) =
        Simulation::try_from_replay(ReplayJson::decode(&bytes).unwrap()).unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..10 {
        if loader.advance_tick(&mut replayed, &mut schedule) {
            assert_eq!(replayed.get_tick(), sim.get_tick());
            assert_eq!(replayed.hashes(), sim.hashes());
            return replayed;
        }
    }
    panic!("replay did not finish");
}

#[test]
fn zone2_uniform_layout_makes_equal_aligned_lots_on_a_straight_road() {
    let mut varied = TestCtx::new();
    varied.apply(&[street()]);
    let map = varied.g.map();
    assert_eq!(map.lot_layout(), LotLayout::Varied);
    let road = road_with_lots(&map);
    assert!(
        distinct_sizes(&map, road).len() > 1,
        "the varied control street should mix lot sizes"
    );
    drop(map);

    let mut uniform = TestCtx::new();
    uniform.apply(&[WorldCommand::SetLotLayout(LotLayout::Uniform), street()]);
    assert_accepted(&uniform.g);
    let map = uniform.g.map();
    assert_eq!(map.lot_layout(), LotLayout::Uniform);
    let road = road_with_lots(&map);
    assert_uniform(&map, road);

    // Lots on both sides of the street start at the same frontage positions.
    let r = map.get(road).unwrap();
    let along = |side: f32| -> Vec<i32> {
        let mut along: Vec<i32> = map
            .lots()
            .values()
            .filter(|lot| lot.parent == road)
            .filter(|lot| (lot.shape.center().y - r.points.first().y).signum() == side)
            .map(|lot| (lot.shape.center().x * 10.0).round() as i32)
            .collect();
        along.sort_unstable();
        along
    };
    let (left, right) = (along(1.0), along(-1.0));
    assert!(!left.is_empty());
    assert_eq!(left, right);
    for pair in left.windows(2) {
        assert!(pair[1] - pair[0] >= (UNIFORM_LOT_SIZE * 10.0) as i32);
    }
}

#[test]
fn zone2_switching_layout_regenerates_only_empty_lots_and_keeps_zoning() {
    let mut test = TestCtx::new();
    test.apply(&[
        street(),
        WorldCommand::ZonePaint {
            area: OBB::new(vec2(250.0, 100.0), Vec2::X, 304.0, 112.0),
            kind: Some(ZoneKind::Residential),
            density: Density::Low,
        },
    ]);
    assert_accepted(&test.g);
    let road = road_with_lots(&test.g.map());
    let house = test.build_house_near(vec2(250.0, 130.0));
    let (house_obb, origin_shape, buildings) = {
        let map = test.g.map();
        (
            map.get(house).unwrap().obb,
            map.lot_origins[&house].shape,
            map.buildings().len(),
        )
    };
    let zones = test.g.read::<ZoneGrid>().clone();

    for layout in [LotLayout::Uniform, LotLayout::Varied, LotLayout::Uniform] {
        test.apply(&[WorldCommand::SetLotLayout(layout)]);
        assert_accepted(&test.g);
        let map = test.g.map();
        assert_eq!(map.lot_layout(), layout);

        // Built lots keep their footprint and the origin used to restore them.
        assert_eq!(map.buildings().len(), buildings);
        assert_eq!(map.get(house).unwrap().obb.corners, house_obb.corners);
        assert_eq!(map.lot_origins[&house].shape.corners, origin_shape.corners);
        assert!(map
            .spatial_map()
            .query(house_obb, ProjectFilter::LOT)
            .next()
            .is_none());

        // Zoning paint is geometry and survives; the new lots read it back.
        let grid = test.g.read::<ZoneGrid>();
        assert_eq!(grid.cells, zones.cells);
        assert_eq!(grid.generation, zones.generation);
        let lots: Vec<_> = map.lots().values().filter(|l| l.parent == road).collect();
        assert!(lots.len() >= 8, "only {} lots regenerated", lots.len());
        assert!(lots.iter().all(|lot| grid.lot_zone(&lot.shape).is_some()));

        match layout {
            LotLayout::Uniform => assert_uniform(&map, road),
            LotLayout::Varied => assert!(distinct_sizes(&map, road).len() > 1),
        }
    }

    // A demolished house restores its original lot, whatever the current layout.
    test.apply(&[WorldCommand::MapRemoveBuilding(house)]);
    assert!(test
        .g
        .map()
        .lots()
        .values()
        .any(|lot| lot.shape.corners == origin_shape.corners));
}

#[test]
fn zone2_set_lot_layout_replays_deterministically() {
    let run = || {
        let mut sim = recorded_sim();
        let z = sim.map().environment.height(vec2(600.0, 600.0)).unwrap();
        let commands = [
            connect(vec3(600.0, 600.0, z), vec3(900.0, 600.0, z)),
            WorldCommand::SetLotLayout(LotLayout::Uniform),
            connect(vec3(600.0, 800.0, z), vec3(900.0, 800.0, z)),
            WorldCommand::SetLotLayout(LotLayout::Uniform),
        ];
        for command in &commands {
            sim.tick(&mut Simulation::schedule(), std::slice::from_ref(command));
            assert_accepted(&sim);
        }
        sim
    };
    let first = run();
    let second = run();
    assert_eq!(first.hashes(), second.hashes());
    assert!(first.hashes().contains_key("lot_layout"));
    let replayed = replay_matches(&first);
    assert_eq!(replayed.map().lot_layout(), LotLayout::Uniform);
    for road in replayed.map().roads().keys() {
        if replayed.map().lots().values().any(|lot| lot.parent == road) {
            assert_uniform(&replayed.map(), road);
        }
    }
}

#[test]
fn zone2_new_game_option_sets_the_layout_and_replays() {
    drop(TestCtx::new());
    assert_eq!(NewGameOptions::default().lot_layout, LotLayout::Varied);
    let options = NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        lot_layout: LotLayout::Uniform,
        ..NewGameOptions::default()
    };
    let mut city = Simulation::new_game_with_replay(&options).unwrap();
    assert_eq!(city.map().lot_layout(), LotLayout::Uniform);
    let ground = city.map().environment.height(vec2(600.0, 600.0)).unwrap();
    city.apply_commands([&connect(
        vec3(600.0, 600.0, ground),
        vec3(900.0, 600.0, ground),
    )]);
    let road = road_with_lots(&city.map());
    assert_uniform(&city.map(), road);
    let replayed = replay_matches(&city);
    assert_eq!(replayed.map().lot_layout(), LotLayout::Uniform);

    let varied = Simulation::new_game(&NewGameOptions {
        lot_layout: LotLayout::Varied,
        ..options
    })
    .unwrap();
    assert_eq!(varied.map().lot_layout(), LotLayout::Varied);
}

#[test]
fn zone2_lot_layout_survives_save_and_load() {
    let mut test = TestCtx::new();
    test.apply(&[WorldCommand::SetLotLayout(LotLayout::Uniform), street()]);
    let bytes = Bincode::encode(&test.g).unwrap();
    let loaded: Simulation = Bincode::decode(&bytes).unwrap();
    assert_eq!(loaded.map().lot_layout(), LotLayout::Uniform);
    assert!(loaded.is_equal(&test.g));
    assert_eq!(loaded.hashes(), test.g.hashes());

    // Later roads in the loaded city keep using the saved layout.
    let mut loaded = loaded;
    loaded.apply_commands([&connect(vec3(100.0, 300.0, 0.0), vec3(400.0, 300.0, 0.0))]);
    for road in loaded.map().roads().keys() {
        if loaded.map().lots().values().any(|lot| lot.parent == road) {
            assert_uniform(&loaded.map(), road);
        }
    }

    // Saves from before the setting load with the varied layout.
    let mut legacy: SimulationDeser = Bincode::decode(&bytes).unwrap();
    assert!(legacy.res.remove("lot_layout").is_some());
    let legacy = legacy.into_sim().unwrap();
    assert_eq!(legacy.map().lot_layout(), LotLayout::Varied);
    assert_eq!(legacy.map().lots().len(), test.g.map().lots().len());
}
